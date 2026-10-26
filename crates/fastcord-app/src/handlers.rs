//! `App::handle_message`, split by message family (#169). Each file holds
//! the arms of one family, moved unchanged; `handle_message` routes to them.

mod call;
mod conversation;
mod desktop;
mod login;
mod navigation;
mod panels;
mod settings;
