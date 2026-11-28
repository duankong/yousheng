use super::Session;
use std::future::Future;
use tokio::sync::{watch, RwLock};
use uuid::Uuid;

pub(super) struct RoomState {
    pub(super) current: RwLock<Option<Session>>,
    intent: watch::Sender<RoomIntent>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RoomIntent {
    Idle,
    Opening(Uuid),
    Closing,
}

pub(super) struct RoomOpening<'a> {
    state: &'a RoomState,
    id: Uuid,
}

impl Default for RoomState {
    fn default() -> Self {
        Self {
            current: RwLock::default(),
            intent: watch::channel(RoomIntent::Idle).0,
        }
    }
}

impl RoomState {
    pub(super) fn begin_opening(&self) -> Result<RoomOpening<'_>, String> {
        let id = Uuid::new_v4();
        let mut rejected = "房间正在切换";
        let started = self.intent.send_if_modified(|intent| {
            if *intent == RoomIntent::Idle {
                *intent = RoomIntent::Opening(id);
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

    pub(super) async fn prepare_join<T>(
        &self,
        host: iroh::EndpointId,
        connect: impl Future<Output = Result<T, String>>,
    ) -> Result<(RoomOpening<'_>, T), String> {
        let opening = {
            let slot = self.current.read().await;
            if slot.as_ref().is_some_and(|session| {
                session.client.is_connected()
                    && session
                        .guest
                        .as_ref()
                        .is_some_and(|guest| guest.host_id() == host)
            }) {
                return Err("你已在这个房间".into());
            }
            self.begin_opening()?
        };
        let candidate = opening.prepare(connect).await?;
        Ok((opening, candidate))
    }

    pub(super) async fn replace(
        &self,
        opening: &RoomOpening<'_>,
        publish: impl FnOnce() -> Session,
    ) -> Result<(), String> {
        let mut slot = self.current.write().await;
        if let Some(session) = opening.publish(|| slot.take())? {
            session.close().await;
        }
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
                if matches!(*intent, RoomIntent::Opening(_)) {
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
        match *self.intent.borrow() {
            RoomIntent::Idle => Ok(()),
            RoomIntent::Opening(_) => Err("房间状态正在变化，请稍后安装更新"),
            RoomIntent::Closing => Err("房间状态已变化，请重新确认安装"),
        }
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
        let session = self.current.write().await.take();
        if let Some(session) = session {
            session.close().await;
        }
    }
}

impl RoomOpening<'_> {
    pub(super) async fn prepare<T>(
        &self,
        connect: impl Future<Output = Result<T, String>>,
    ) -> Result<T, String> {
        let mut intent = self.state.intent.subscribe();
        tokio::select! {
            biased;
            Ok(changed) = intent.wait_for(|intent| *intent != RoomIntent::Opening(self.id)) =>
                Err(Self::cancellation(*changed).into()),
            result = connect => result,
        }
    }

    pub(super) fn publish<T>(&self, publish: impl FnOnce() -> T) -> Result<T, String> {
        let intent = self.state.intent.borrow();
        if *intent != RoomIntent::Opening(self.id) {
            return Err(Self::cancellation(*intent).into());
        }
        let result = publish();
        drop(intent);
        Ok(result)
    }

    fn cancellation(intent: RoomIntent) -> &'static str {
        if intent == RoomIntent::Closing {
            "软件正在退出"
        } else {
            "房间已切换"
        }
    }
}

impl Drop for RoomOpening<'_> {
    fn drop(&mut self) {
        self.state.intent.send_if_modified(|intent| {
            if *intent == RoomIntent::Opening(self.id) {
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
    async fn pending_join_does_not_block_exiting_the_active_room() {
        let rooms = RoomState::default();
        *rooms.current.write().await = Some(session("active"));
        let mut joining = Box::pin(rooms.prepare_join(host(), pending::<Result<(), String>>()));
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
        let (first, ()) = rooms.prepare_join(host(), ready(Ok(()))).await.unwrap();
        rooms.leave("active").await.unwrap();
        let (second, ()) = rooms.prepare_join(host(), ready(Ok(()))).await.unwrap();
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
        let (opening, ()) = rooms.prepare_join(host(), ready(Ok(()))).await.unwrap();
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

        let result = rooms
            .prepare_join(host(), ready(Err::<(), _>("admission failed".into())))
            .await;

        assert_eq!(result.err().as_deref(), Some("admission failed"));
        assert_eq!(rooms.current.read().await.as_ref().unwrap().id, "active");
        assert!(rooms.check_install_ready().is_ok());
    }
}
