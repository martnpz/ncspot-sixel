//! Cover rendering through an external ueberzug(pp) process.

use std::io::Write;
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::RwLock;

use cursive::Vec2;
use log::error;

use super::CoverBackend;

pub struct UeberzugBackend {
    process: RwLock<Option<Child>>,
}

impl UeberzugBackend {
    pub fn new() -> Self {
        Self {
            process: RwLock::new(None),
        }
    }

    fn run_cmd(&self, cmd: &str) -> Result<(), std::io::Error> {
        let mut process = self.process.write().unwrap();

        if process.is_none() {
            *process = Some(
                std::process::Command::new("ueberzug")
                    .args(["layer", "--silent"])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()?,
            );
        }

        let stdin = (*process).as_mut().unwrap().stdin.as_mut().unwrap();
        stdin.write_all(cmd.as_bytes())?;

        Ok(())
    }
}

impl CoverBackend for UeberzugBackend {
    fn draw(
        &self,
        _url: &str,
        path: &Path,
        mut draw_offset: Vec2,
        draw_size: Vec2,
        font_size: Vec2,
    ) -> bool {
        let mut img_size = Vec2::new(640, 640);

        let draw_size_pxls = draw_size * font_size;
        let ratio = f32::min(
            f32::min(
                draw_size_pxls.x as f32 / img_size.x as f32,
                draw_size_pxls.y as f32 / img_size.y as f32,
            ),
            1.0,
        );

        img_size = Vec2::new(
            (ratio * img_size.x as f32) as usize,
            (ratio * img_size.y as f32) as usize,
        );

        // Ueberzug takes an area given in chars and fits the image to
        // that area (from the top left). Since we want to center the
        // image at least horizontally, we need to fiddle around a bit.
        let mut size = img_size / font_size;

        // Make sure there is equal space in chars on either side
        if size.x % 2 != draw_size.x % 2 {
            size.x -= 1;
        }

        // Make sure x is the bottleneck so full width is used
        size.y = std::cmp::min(draw_size.y, size.y + 1);

        // Round up since the bottom might have empty space within
        // the designated box
        draw_offset.x += (draw_size.x - size.x) / 2;
        draw_offset.y += (draw_size.y - size.y) - (draw_size.y - size.y) / 2;

        let cmd = format!(
            "{{\"action\":\"add\",\"scaler\":\"fit_contain\",\"identifier\":\"cover\",\"x\":{},\"y\":{},\"width\":{},\"height\":{},\"path\":\"{}\"}}\n",
            draw_offset.x,
            draw_offset.y,
            size.x,
            size.y,
            path.to_str().unwrap()
        );

        if let Err(e) = self.run_cmd(&cmd) {
            error!("Failed to run Ueberzug: {e}");
            return false;
        }
        true
    }

    fn clear(&self, _offset: Vec2, _size: Vec2) {
        let cmd = "{\"action\": \"remove\", \"identifier\": \"cover\"}\n";
        if let Err(e) = self.run_cmd(cmd) {
            error!("Failed to run Ueberzug: {e}");
        }
    }
}
