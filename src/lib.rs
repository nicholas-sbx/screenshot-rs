//! Flatshot: ShareX-style region screenshots for Wayland and X11 (KDE first) with a
//! floating draw toolbar, QR/barcode scanning and screen recording.

pub mod app;
pub mod capture;
pub mod cli;
pub mod clipboard;
pub mod codec;
pub mod config;
pub mod dbus;
pub mod deliver;
pub mod desktop;
pub mod diagnose;
pub mod geom;
pub mod image;
pub mod ipc;
pub mod keys;
pub mod keyseq;
pub mod mem;
pub mod platform;
pub mod record;
pub mod render;
pub mod scanner;
pub mod selftest;
pub mod signals;
pub mod template;
pub mod timing;
pub mod ui;
pub mod util;
pub mod wayland;
pub mod x11;
