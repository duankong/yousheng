use super::Session;
use std::{future::Future, sync::Arc};
use tokio::sync::{watch, Mutex, MutexGuard, RwLock};

pub(super) struct RoomState {
    pub(super) current: RwLock<Option<Session>>,
    intent: watch::Sender<RoomIntent>,
    host_start: Mutex<()>,
}

#[derive(PartialEq, Eq)]
enum RoomIntent {
    Idle,
    Hosting(Arc<str>),
    Joining(Arc<str>),
    CancellingJoin(Arc<str>),
    HandingOff(Arc<str>),
    Installing,
    Closing,
}

pub(super) struct RoomOpening<'a> {
    state: &'a RoomState,
    id: Arc<str>,
}

pub(super) struct RoomInstall<'a>(&'a RoomState);

impl Default for RoomState {
    fn default() -> Self {
        Self {
            current: RwLock::default(),
            intent: watch::channel(RoomIntent::Idle).0,
            host_start: Mutex::default(),
        }
    }
}

impl RoomState {
    fn begin_opening(
        &self,
        id: &str,
        opening: impl FnOnce(Arc<str>) -> RoomIntent,
    ) -> Result<RoomOpening<'_>, String> {
        let id = Arc::from(id);
        let mut rejected = "房间正在切换";
        let started = self.intent.send_if_modified(|intent| {
            if *intent == RoomIntent::Idle {
                *intent = opening(Arc::clone(&id));
                true
            } else {
                if *intent == RoomIntent::Closing {
                    rejected = "软件正在退出";
                }
                false
            }
        });
        if started {
            Ok(RoomOpening { state: self, id })
        } else {
            Err(rejected.into())
        }
    }

    pub(super) async fn begin_host(
        &self,
        id: &str,
    ) -> Result<(MutexGuard<'_, ()>, RoomOpening<'_>), String> {
        let host_start = self.host_start.try_lock().map_err(|_| "房间正在切换")?;
        let _slot = self.current.read().await;
        let opening = self.begin_opening(id, RoomIntent::Hosting)?;
        Ok((host_start, opening))
    }

    pub(super) async fn begin_join(
        &self,
        id: &str,
        host: iroh::EndpointId,
    ) -> Result<RoomOpening<'_>, String> {
        let opening = self.begin_opening(id, RoomIntent::Joining)?;
        let slot = opening
            .prepare(async { Ok(self.current.read().await) })
            .await?;
        if slot.as_ref().is_some_and(|session| {
            session.client.is_connected()
                && session
                    .guest
                    .as_ref()
                    .is_some_and(|guest| guest.host_id() == host)
        }) {
            return Err("你已在这个房间".into());
        }
        Ok(opening)
    }

    pub(super) fn cancel_join(&self, session_id: &str) -> bool {
        self.intent.send_if_modified(|intent| {
            if let RoomIntent::Joining(id) = intent {
                if id.as_ref() == session_id {
                    *intent = RoomIntent::CancellingJoin(Arc::clone(id));
                    return true;
                }
            }
            false
        })
    }

    pub(super) async fn replace(
        &self,
        opening: &RoomOpening<'_>,
        publish: impl FnOnce() -> Session,
    ) -> Result<(), String> {
        let mut slot = opening
            .prepare(async { Ok(self.current.write().await) })
            .await?;
        opening.drain(&mut slot).await?;
        opening.publish(|| *slot = Some(publish()))
    }

    pub(super) async fn leave(&self, session_id: &str) -> Result<(), String> {
        let mut slot = self.current.write().await;
        if slot
            .as_ref()
            .is_some_and(|session| session.id != session_id)
        {
            return Err("房间已切换".into());
        }
        if let Some(session) = slot.take() {
            self.intent.send_if_modified(|intent| {
                if matches!(*intent, RoomIntent::Joining(_)) {
                    *intent = RoomIntent::Idle;
                    true
                } else {
                    false
                }
            });
            session.close().await;
        }
        Ok(())
    }

    pub(super) fn check_install_ready(&self) -> Result<(), &'static str> {
        Self::install_ready(&self.intent.borrow())
    }

    fn install_ready(intent: &RoomIntent) -> Result<(), &'static str> {
        match intent {
            RoomIntent::Idle => Ok(()),
            RoomIntent::Closing => Err("房间状态已变化，请重新确认安装"),
            _ => Err("房间状态正在变化，请稍后安装更新"),
        }
    }

    pub(super) fn begin_install(&self) -> Result<RoomInstall<'_>, &'static str> {
        let mut ready = Ok(());
        self.intent.send_if_modified(|intent| {
            ready = Self::install_ready(intent);
            if ready.is_ok() {
                *intent = RoomIntent::Installing;
                true
            } else {
                false
            }
        });
        ready.map(|()| RoomInstall(self))
    }

    pub(super) fn begin_shutdown(&self) -> bool {
        self.intent.send_if_modified(|intent| {
            if *intent == RoomIntent::Closing {
                false
            } else {
                *intent = RoomIntent::Closing;
                true
            }
        })
    }

    pub(super) async fn shutdown(&self) {
        let _host_start = self.host_start.lock().await;
        let session = self.current.write().await.take();
        if let Some(session) = session {
            session.close().await;
        }
    }
}

impl RoomInstall<'_> {
    pub(super) fn check_ready(&self) -> Result<(), &'static str> {
        if *self.0.intent.borrow() == RoomIntent::Installing {
            Ok(())
        } else {
            Err("房间状态已变化，请重新确认安装")
        }
    }
}

impl Drop for RoomInstall<'_> {
    fn drop(&mut self) {
        self.0.intent.send_if_modified(|intent| {
            if *intent == RoomIntent::Installing {
                *intent = RoomIntent::Idle;
                true
            } else {
                false
            }
        });
    }
}

impl RoomOpening<'_> {
    pub(super) async fn drain(&self, slot: &mut Option<Session>) -> Result<(), String> {
        let mut session = None;
        let mut result: Result<(), String> = Ok(());
        self.state.intent.send_if_modified(|intent| {
            if matches!(intent, RoomIntent::Hosting(id) | RoomIntent::Joining(id) if Arc::ptr_eq(id, &self.id))
            {
                session = slot.take();
                *intent = RoomIntent::HandingOff(self.id.clone());
                true
            } else {
                result = Err(Self::cancellation(intent).into());
                false
            }
        });
        result?;
        if let Some(session) = session {
            session.close().await;
        }
        Ok(())
    }

    pub(super) async fn prepare<T>(
        &self,
        connect: impl Future<Output = Result<T, String>>,
    ) -> Result<T, String> {
        let mut intent = self.state.intent.subscribe();
        tokio::select! {
            biased;
            Ok(changed) = intent.wait_for(|intent| !self.is_opening(intent)) =>
                Err(Self::cancellation(&changed).into()),
            result = connect => result,
        }
    }

    pub(super) fn publish<T>(&self, publish: impl FnOnce() -> T) -> Result<T, String> {
        let intent = self.state.intent.borrow();
        if !matches!(&*intent, RoomIntent::HandingOff(id) if Arc::ptr_eq(id, &self.id)) {
            return Err(Self::cancellation(&intent).into());
        }
        let result = publish();
        drop(intent);
        Ok(result)
    }

    fn is_opening(&self, intent: &RoomIntent) -> bool {
        matches!(intent, RoomIntent::Hosting(id) | RoomIntent::Joining(id) | RoomIntent::HandingOff(id) if Arc::ptr_eq(id, &self.id))
    }

    fn cancellation(intent: &RoomIntent) -> &'static str {
        match intent {
            RoomIntent::Closing => "软件正在退出",
            RoomIntent::CancellingJoin(_) => "已取消加入房间",
            _ => "房间已切换",
        }
    }
}

impl Drop for RoomOpening<'_> {
    fn drop(&mut self) {
        self.state.intent.send_if_modified(|intent| {
            if self.is_opening(intent)
                || matches!(intent, RoomIntent::CancellingJoin(id) if Arc::ptr_eq(id, &self.id))
            {
                *intent = RoomIntent::Idle;
                true
            } else {
                false
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        client::tests::{command_peer, pause_audio_cleanup},
        diagnostics::{Diagnostics, Role},
    };
    use std::{
        future::{pending, ready},
        task::Poll,
    };

    fn session(id: &str) -> Session {
        Session {
            id: id.into(),
            client: command_peer().0,
            host: None,
            guest: None,
            diagnostics: Diagnostics::default().room(Role::Host),
        }
    }

    fn host() -> iroh::EndpointId {
        iroh::SecretKey::from_bytes(&[42; 32]).public()
    }

    #[tokio::test]
    async fn cancel_only_matches_the_pending_join_and_keeps_the_active_room() {
        let rooms = RoomState::default();
        *rooms.current.write().await = Some(session("active"));
        let opening = rooms.begin_join("joining", host()).await.unwrap();
        let mut admission = Box::pin(opening.prepare(pending::<Result<(), String>>()));
        assert!(futures_util::poll!(&mut admission).is_pending());
        assert!(!rooms.cancel_join("active"));
        assert!(!rooms.cancel_join("different"));
        assert!(futures_util::poll!(&mut admission).is_pending());

        assert!(rooms.cancel_join("joining"));
        assert!(!rooms.cancel_join("joining"));
        assert_eq!(admission.await.unwrap_err(), "已取消加入房间");
        assert_eq!(rooms.current.read().await.as_ref().unwrap().id, "active");
    }

    #[tokio::test]
    async fn cancel_stops_join_waiting_for_the_first_room_read() {
        let rooms = RoomState::default();
        let mut slot = rooms.current.write().await;
        *slot = Some(session("active"));
        let mut joining = Box::pin(rooms.begin_join("joining", host()));
        assert!(futures_util::poll!(&mut joining).is_pending());

        assert!(rooms.cancel_join("joining"));
        assert!(matches!(
            futures_util::poll!(&mut joining),
            Poll::Ready(Err(ref error)) if error == "已取消加入房间"
        ));
        assert_eq!(slot.as_ref().unwrap().id, "active");
        assert!(rooms.check_install_ready().is_ok());
    }

    #[tokio::test]
    async fn reserved_install_blocks_new_joins_and_allows_shutdown() {
        let rooms = RoomState::default();
        let _slot = rooms.current.write().await;
        let install = rooms.begin_install().unwrap();
        assert!(rooms.begin_join("joining", host()).await.is_err());
        assert!(!rooms.cancel_join("joining"));
        assert!(install.check_ready().is_ok());

        assert!(rooms.begin_shutdown());
        assert!(install.check_ready().is_err());
        drop(install);
        assert!(!rooms.begin_shutdown());
    }

    #[tokio::test]
    async fn cancel_after_admission_before_handoff_keeps_the_active_room() {
        let rooms = RoomState::default();
        *rooms.current.write().await = Some(session("active"));
        let opening = rooms.begin_join("joining", host()).await.unwrap();
        opening.prepare(ready(Ok(()))).await.unwrap();

        assert!(rooms.cancel_join("joining"));
        assert!(rooms
            .replace(&opening, || panic!("canceled admission was activated"))
            .await
            .is_err());
        assert_eq!(rooms.current.read().await.as_ref().unwrap().id, "active");
    }

    #[tokio::test]
    async fn cancel_after_handoff_starts_allows_the_candidate_to_be_published() {
        let rooms = RoomState::default();
        let mut active = session("active");
        let allow_cleanup = pause_audio_cleanup(&mut active.client);
        *rooms.current.write().await = Some(active);
        let opening = rooms.begin_join("joining", host()).await.unwrap();
        opening.prepare(ready(Ok(()))).await.unwrap();
        let mut replacing = Box::pin(rooms.replace(&opening, || session("joining")));
        assert!(futures_util::poll!(&mut replacing).is_pending());

        assert!(!rooms.cancel_join("joining"));
        assert!(rooms.check_install_ready().is_err());
        allow_cleanup.send(()).unwrap();
        replacing.await.unwrap();
        assert!(!rooms.cancel_join("joining"));
        assert_eq!(rooms.current.read().await.as_ref().unwrap().id, "joining");
    }

    #[tokio::test]
    async fn pending_join_does_not_block_exiting_the_active_room() {
        let rooms = RoomState::default();
        *rooms.current.write().await = Some(session("active"));
        let opening = rooms.begin_join("joining", host()).await.unwrap();
        let mut joining = Box::pin(opening.prepare(pending::<Result<(), String>>()));
        assert!(futures_util::poll!(&mut joining).is_pending());
        assert!(rooms.current.try_write().is_ok());
        assert_eq!(
            rooms.check_install_ready(),
            Err("房间状态正在变化，请稍后安装更新")
        );

        assert!(rooms.begin_shutdown());
        assert!(
            matches!(futures_util::poll!(&mut joining), Poll::Ready(Err(_))),
            "the candidate should stop waiting once application exit is requested"
        );
        rooms.shutdown().await;
        assert!(rooms.current.read().await.is_none());
    }

    #[tokio::test]
    async fn late_admission_cannot_replace_a_newer_room() {
        let rooms = RoomState::default();
        *rooms.current.write().await = Some(session("active"));
        let first = rooms.begin_join("joining", host()).await.unwrap();
        first.prepare(ready(Ok(()))).await.unwrap();
        rooms.leave("active").await.unwrap();
        let second = rooms.begin_join("joining", host()).await.unwrap();
        second.prepare(ready(Ok(()))).await.unwrap();
        rooms.replace(&second, || session("next")).await.unwrap();

        assert!(rooms
            .replace(&first, || panic!("stale admission was activated"))
            .await
            .is_err());
        assert_eq!(rooms.current.read().await.as_ref().unwrap().id, "next");
    }

    #[tokio::test]
    async fn shutdown_during_old_audio_cleanup_prevents_new_room_and_update() {
        let rooms = RoomState::default();
        let mut active = session("active");
        let allow_cleanup = pause_audio_cleanup(&mut active.client);
        *rooms.current.write().await = Some(active);
        let opening = rooms.begin_join("joining", host()).await.unwrap();
        opening.prepare(ready(Ok(()))).await.unwrap();
        let mut replacing =
            Box::pin(rooms.replace(&opening, || panic!("room activated after exit")));
        assert!(futures_util::poll!(&mut replacing).is_pending());
        assert!(rooms.current.try_read().is_err());

        assert!(rooms.begin_shutdown());
        assert_eq!(
            rooms.check_install_ready(),
            Err("房间状态已变化，请重新确认安装")
        );
        allow_cleanup.send(()).unwrap();
        assert_eq!(replacing.await.unwrap_err(), "软件正在退出");
        rooms.shutdown().await;
        assert!(rooms.current.read().await.is_none());
    }

    #[tokio::test]
    async fn failed_join_keeps_the_active_room() {
        let rooms = RoomState::default();
        *rooms.current.write().await = Some(session("active"));

        let opening = rooms.begin_join("joining", host()).await.unwrap();
        let result = opening
            .prepare(ready(Err::<(), _>("admission failed".into())))
            .await;
        drop(opening);

        assert_eq!(result.err().as_deref(), Some("admission failed"));
        assert_eq!(rooms.current.read().await.as_ref().unwrap().id, "active");
        assert!(rooms.check_install_ready().is_ok());
    }

    #[tokio::test]
    async fn host_start_allows_room_access_and_shutdown_waits_for_cleanup() {
        let rooms = RoomState::default();
        let (host_start, opening) = rooms.begin_host("hosting").await.unwrap();
        opening
            .drain(&mut *rooms.current.write().await)
            .await
            .unwrap();
        let mut starting = Box::pin(opening.prepare(pending::<Result<(), String>>()));
        assert!(futures_util::poll!(&mut starting).is_pending());
        assert!(rooms.current.try_write().is_ok());
        assert!(!rooms.cancel_join("hosting"));
        assert!(rooms.check_install_ready().is_err());

        assert!(rooms.begin_shutdown());
        assert_eq!(starting.await.unwrap_err(), "软件正在退出");
        let mut shutdown = Box::pin(rooms.shutdown());
        assert!(futures_util::poll!(&mut shutdown).is_pending());
        drop(opening);
        drop(host_start);
        shutdown.await;
    }
}
