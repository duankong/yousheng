#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod channels;
mod chat;
mod client;
mod host;
mod ice;
mod lobby;
mod network;
mod preferences;
#[cfg(test)]
mod scenario_tests;
mod voice;

fn main() {
    app::run();
}
