//! Standard user folders and mounted drives for the sidebar.

use std::path::PathBuf;

use egui_phosphor::regular as icon;

pub struct Place {
    pub label: String,
    pub icon: &'static str,
    pub path: PathBuf,
}

pub fn home_dir() -> PathBuf {
    directories::UserDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(if cfg!(windows) { "C:\\" } else { "/" }))
}

pub fn standard_places() -> Vec<Place> {
    let Some(dirs) = directories::UserDirs::new() else {
        return Vec::new();
    };
    let mut places = vec![Place {
        label: "Home".to_owned(),
        icon: icon::HOUSE,
        path: dirs.home_dir().to_path_buf(),
    }];
    let optional = [
        ("Desktop", icon::DESKTOP, dirs.desktop_dir()),
        ("Documents", icon::FILE_TEXT, dirs.document_dir()),
        ("Downloads", icon::DOWNLOAD_SIMPLE, dirs.download_dir()),
        ("Pictures", icon::IMAGE, dirs.picture_dir()),
        ("Music", icon::MUSIC_NOTES, dirs.audio_dir()),
        ("Videos", icon::FILM_STRIP, dirs.video_dir()),
    ];
    places.extend(optional.into_iter().filter_map(|(label, icon, path)| {
        path.filter(|p| p.is_dir()).map(|p| Place {
            label: label.to_owned(),
            icon,
            path: p.to_path_buf(),
        })
    }));
    places
}

#[cfg(windows)]
pub fn drives() -> Vec<Place> {
    ('A'..='Z')
        .map(|letter| PathBuf::from(format!("{letter}:\\")))
        .filter(|p| p.exists())
        .map(|path| Place {
            label: path.display().to_string(),
            icon: icon::HARD_DRIVE,
            path,
        })
        .collect()
}

#[cfg(not(windows))]
pub fn drives() -> Vec<Place> {
    let mut drives = vec![Place {
        label: "File System".to_owned(),
        icon: icon::HARD_DRIVE,
        path: PathBuf::from("/"),
    }];
    let user = std::env::var("USER").unwrap_or_default();
    let roots = [
        PathBuf::from("/run/media").join(&user),
        PathBuf::from("/media").join(&user),
        PathBuf::from("/mnt"),
    ];
    for root in roots {
        let Ok(read) = std::fs::read_dir(&root) else {
            continue;
        };
        let mut found: Vec<_> = read
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .map(|path| Place {
                label: crate::fs::display_name(&path),
                icon: icon::HARD_DRIVE,
                path,
            })
            .collect();
        found.sort_by(|a, b| a.label.cmp(&b.label));
        drives.extend(found);
    }
    drives
}
