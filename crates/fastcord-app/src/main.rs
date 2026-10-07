#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use iced::widget::{center, text};
use iced::{Element, Task};

fn main() -> iced::Result {
    iced::application(App::default, App::update, App::view)
        .title("fastcord")
        .run()
}

#[derive(Default)]
struct App;

#[derive(Debug, Clone)]
enum Message {}

impl App {
    fn update(&mut self, message: Message) -> Task<Message> {
        match message {}
    }

    fn view(&self) -> Element<'_, Message> {
        center(text("fastcord")).into()
    }
}
