//! Stand-in screens: `tests/screens/<agent>/<status>-<label>.txt`, with optional
//! `@title …` / `@progress …` header lines.
#![allow(dead_code)] // each test binary uses part of it

use std::path::{Path, PathBuf};
use tmuxedo_detect::{Input, Status};

pub struct Screen {
    pub path: PathBuf,
    pub want: Status,
    pub screen: String,
    pub title: String,
    pub progress: String,
}

impl Screen {
    pub fn input(&self) -> Input<'_> {
        Input {
            screen: &self.screen,
            title: &self.title,
            progress: &self.progress,
        }
    }
}

pub fn screens_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/screens")
}

pub fn screens(agent: &str) -> Vec<Screen> {
    let dir = screens_dir().join(agent);
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|_| panic!("{dir:?}"))
        .flatten()
        .map(|e| e.path())
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            let want =
                Status::parse(name.split('-').next().unwrap()).unwrap_or_else(|| panic!("{name}: status prefix"));
            let raw = std::fs::read_to_string(&path).unwrap();
            let (mut title, mut progress, mut lines) = (String::new(), String::new(), vec![]);
            for line in raw.lines() {
                if let Some(t) = line.strip_prefix("@title ") {
                    title = t.into();
                } else if let Some(p) = line.strip_prefix("@progress ") {
                    progress = p.into();
                } else {
                    lines.push(line);
                }
            }
            Screen {
                path,
                want,
                screen: lines.join("\n"),
                title,
                progress,
            }
        })
        .collect()
}
