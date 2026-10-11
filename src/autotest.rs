//! Scripted UI driver for automated checks (`--features autotest`).
//!
//! `DIRMERGE_SCRIPT="wait 1; key alt+ArrowDown; click 400 300; shot out.png; quit"`
//!
//! Commands: `wait <sec>`, `key <mods+Name>`, `click x y`, `dclick x y`,
//! `rclick x y`, `move x y`, `scroll <dy>`, `text <str>`, `shot <file.png>`, `quit`.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use egui::{Event, Key, Modifiers, MouseWheelUnit, PointerButton, Pos2, TouchPhase, pos2, vec2};

pub struct Driver {
    cmds: VecDeque<String>,
    resume_at: Instant,
    queued: VecDeque<Vec<Event>>,
    shot_path: Option<String>,
    pointer: Pos2,
}

impl Driver {
    pub fn from_env() -> Option<Driver> {
        let script = std::env::var("DIRMERGE_SCRIPT").ok()?;
        Some(Driver {
            cmds: script.split(';').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
            resume_at: Instant::now() + Duration::from_millis(800),
            queued: VecDeque::new(),
            shot_path: None,
            pointer: pos2(10.0, 10.0),
        })
    }

    pub fn hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        // Collect screenshot results.
        for e in &raw.events {
            if let Event::Screenshot { image, .. } = e
                && let Some(path) = self.shot_path.take()
            {
                save_png(&path, image);
            }
        }
        ctx.request_repaint();
        if let Some(evs) = self.queued.pop_front() {
            raw.events.extend(evs);
            return;
        }
        if self.shot_path.is_some() || Instant::now() < self.resume_at {
            return;
        }
        let Some(cmd) = self.cmds.pop_front() else { return };
        let mut parts = cmd.splitn(2, ' ');
        let op = parts.next().unwrap_or("");
        let arg = parts.next().unwrap_or("").trim();
        let xy = || -> Pos2 {
            let v: Vec<f32> = arg.split_whitespace().filter_map(|s| s.parse().ok()).collect();
            pos2(v.first().copied().unwrap_or(0.0), v.get(1).copied().unwrap_or(0.0))
        };
        match op {
            "wait" => self.resume_at = Instant::now() + Duration::from_secs_f32(arg.parse().unwrap_or(0.5)),
            "shot" => {
                self.shot_path = Some(arg.to_string());
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
            }
            "quit" => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            "move" => {
                self.pointer = xy();
                raw.events.push(Event::PointerMoved(self.pointer));
            }
            "click" | "dclick" | "rclick" => {
                let p = xy();
                self.pointer = p;
                let button = if op == "rclick" { PointerButton::Secondary } else { PointerButton::Primary };
                let press = |pressed| Event::PointerButton { pos: p, button, pressed, modifiers: Modifiers::NONE };
                raw.events.push(Event::PointerMoved(p));
                self.queued.push_back(vec![press(true)]);
                self.queued.push_back(vec![press(false)]);
                if op == "dclick" {
                    self.queued.push_back(vec![press(true)]);
                    self.queued.push_back(vec![press(false)]);
                }
                self.resume_at = Instant::now() + Duration::from_millis(300);
            }
            "scroll" => {
                let dy: f32 = arg.parse().unwrap_or(-100.0);
                raw.events.push(Event::PointerMoved(self.pointer));
                raw.events.push(Event::MouseWheel {
                    unit: MouseWheelUnit::Point,
                    delta: vec2(0.0, dy),
                    phase: TouchPhase::Move,
                    modifiers: Modifiers::NONE,
                });
                self.resume_at = Instant::now() + Duration::from_millis(400);
            }
            "text" => raw.events.push(Event::Text(arg.to_string())),
            "key" => {
                let mut mods = Modifiers::NONE;
                let mut key = None;
                for part in arg.split('+') {
                    match part.to_lowercase().as_str() {
                        "alt" => mods.alt = true,
                        "shift" => mods.shift = true,
                        "ctrl" => mods.ctrl = true,
                        "cmd" => {
                            mods.command = true;
                            if cfg!(target_os = "macos") { mods.mac_cmd = true } else { mods.ctrl = true }
                        }
                        _ => key = Key::from_name(part),
                    }
                }
                if let Some(key) = key {
                    let ev = |pressed| Event::Key { key, physical_key: None, pressed, repeat: false, modifiers: mods };
                    raw.events.push(ev(true));
                    self.queued.push_back(vec![ev(false)]);
                }
                self.resume_at = Instant::now() + Duration::from_millis(250);
            }
            _ => eprintln!("autotest: unknown command {cmd}"),
        }
    }
}

fn save_png(path: &str, img: &egui::ColorImage) {
    let file = match std::fs::File::create(path) {
        Ok(f) => f,
        Err(e) => return eprintln!("autotest: {path}: {e}"),
    };
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), img.size[0] as u32, img.size[1] as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let data: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
    match enc.write_header().and_then(|mut w| w.write_image_data(&data)) {
        Ok(()) => eprintln!("autotest: saved {path}"),
        Err(e) => eprintln!("autotest: {path}: {e}"),
    }
}
