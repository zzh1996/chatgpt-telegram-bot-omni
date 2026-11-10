//! Render Markdown from stdin to Telegram rich blocks (JSON), for debugging:
//! `cargo run -p omni-markdown --example render < file.md`
use std::io::Read;

fn main() {
    let mut s = String::new();
    std::io::stdin().read_to_string(&mut s).unwrap();
    let blocks = omni_markdown::rich::markdown_to_blocks(&s, 0, omni_markdown::rich::RichLimits::TELEGRAM);
    println!("{}", serde_json::to_string_pretty(&blocks).unwrap());
}
