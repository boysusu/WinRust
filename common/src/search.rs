//! Client of the rust-search service (rs.exe --serve): one named pipe per indexed drive. Shared by rs, rs-launcher and topbar,
//! so asking for files never needs a new process. When Everything's tray or window (not only its service) is already running,
//! ask_all uses that index instead and falls back to the pipes if IPC fails.
use std::{
    fs::OpenOptions,
    io::{self, Read, Write},
    sync::Mutex,
};
use everything_ipc::wm::{EverythingClient, RequestFlags};
use windows_sys::Win32::Storage::FileSystem::GetLogicalDrives;

/// Same cap rust-search uses when serving a pipe reply.
pub const SHOW: usize = 100;

pub fn pipe_name(d: char) -> String {
    format!("\\\\.\\pipe\\rs-search-{d}")
}

/// Letters of all mounted drives. Cheap: no volume is touched, so sleeping disks stay asleep.
pub fn drive_letters() -> Vec<char> {
    let mask = unsafe { GetLogicalDrives() };
    (0..26).filter(|i| mask & (1 << i) != 0).map(|i| (b'A' + i as u8) as char).collect()
}

/// Asks drive `d`'s service. None = no service for that drive (pipe missing).
pub fn ask(d: char, q: &str) -> Option<io::Result<String>> {
    let mut p = match OpenOptions::new().read(true).write(true).open(pipe_name(d)) {
        Ok(p) => p,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
        Err(e) => return Some(Err(e)),
    };
    Some((|| {
        p.write_all(q.as_bytes())?;
        let (mut out, mut chunk) = (Vec::new(), [0u8; 1 << 16]);
        loop {
            match p.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&chunk[..n]),
                // the server ends its reply by hanging up: BROKEN_PIPE (109) / NOT_CONNECTED (233) is EOF
                Err(e) if matches!(e.raw_os_error(), Some(109 | 233)) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    })())
}

/// Hits from Everything or the rust-search pipes. `more` is how many results were cut after [`SHOW`].
/// `indexed` is true when an index answered (even if it found nothing).
pub struct Hits {
    pub paths: Vec<String>,
    pub more: usize,
    pub everything: bool,
    pub indexed: bool,
}

/// Result paths from every drive's service (the "... N more" line dropped), or from Everything when it is running.
/// Empty when no index is available.
pub fn ask_all(q: &str) -> Vec<String> {
    ask_hits(q).paths
}

/// Like [`ask_all`], but keeps the overflow count and which index answered.
pub fn ask_hits(q: &str) -> Hits {
    if let Some(hits) = everything(q) {
        return hits;
    }
    rust_search(q)
}

fn rust_search(q: &str) -> Hits {
    let mut paths = Vec::new();
    let mut more: usize = 0;
    let mut indexed = false;
    for d in drive_letters() {
        let Some(reply) = ask(d, q) else { continue };
        indexed = true;
        let Ok(s) = reply else { continue };
        more = more.saturating_add(take_lines(&s, &mut paths));
    }
    Hits { paths, more, everything: false, indexed }
}

/// Collects paths from a rust-search pipe reply. Returns the "... N more" count (0 if none).
fn take_lines(s: &str, paths: &mut Vec<String>) -> usize {
    let mut more: usize = 0;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("... ") {
            if let Some(n) = rest.split_whitespace().next().and_then(|x| x.parse().ok()) {
                more = more.saturating_add(n);
            }
            continue;
        }
        if !line.is_empty() {
            paths.push(line.to_string());
        }
    }
    more
}

const START_MENU: &str = "\\start menu\\programs\\";
const JUNK: [&str; 2] = ["\\winsxs\\", "\\windowsapps\\"];

/// Start Menu shortcuts, other .lnk, .exe, then the rest; WinSxS / WindowsApps last in a tie, then shorter paths.
pub fn rank_paths(paths: impl IntoIterator<Item = String>, first_term: Option<&str>) -> Vec<String> {
    let term = first_term.map(|t| t.to_lowercase()).filter(|t| !t.is_empty());
    let mut rows: Vec<(u8, u8, u8, usize, String)> = paths
        .into_iter()
        .map(|p| {
            let l = p.to_lowercase();
            let name = l.rsplit('\\').next().unwrap_or(&l);
            let rank = match &term {
                Some(t) if name == t => 0,
                Some(t) if name.starts_with(t) => 1,
                _ => 2,
            };
            let tier = if l.ends_with(".lnk") {
                if l.contains(START_MENU) { 0 } else { 1 }
            } else if l.ends_with(".exe") {
                2
            } else {
                3
            };
            let junk = JUNK.iter().any(|j| l.contains(j)) as u8;
            (tier, rank, junk, p.len(), p)
        })
        .collect();
    rows.sort_unstable();
    rows.into_iter().map(|t| t.4).collect()
}

/// Queries a running Everything tray/UI client (IPC). None if it is not running, its DB is not ready, or the query fails.
/// Everything 1.5's default instance name `1.5a` is tried after the unnamed instance (`EverythingClient::new` does both).
fn everything(q: &str) -> Option<Hits> {
    // SDK v2 keeps process-wide query state: one query at a time, and reuse one client so its reply window stays put.
    static CLIENT: Mutex<Option<EverythingClient>> = Mutex::new(None);
    let mut slot = CLIENT.lock().unwrap_or_else(|e| e.into_inner());
    let ready = |c: &EverythingClient| c.is_ipc_available() && c.is_db_loaded();
    match slot.as_ref() {
        Some(c) if ready(c) => {}
        _ => {
            *slot = EverythingClient::new().ok().filter(ready);
            if slot.is_none() {
                return None;
            }
        }
    }
    let client = slot.as_ref().unwrap();
    let list = match client.query_wait(q).request_flags(RequestFlags::FullPathAndFileName).max_results(SHOW as u32).call() {
        Ok(list) => list,
        Err(_) => {
            *slot = None;
            return None;
        }
    };
    let total = list.total_len();
    let mut paths = Vec::with_capacity(list.len());
    for item in list.iter() {
        if let Some(p) = item.get_string(RequestFlags::FullPathAndFileName) {
            if !p.is_empty() {
                paths.push(p);
            }
        }
    }
    let first = q.split_whitespace().next();
    let paths = rank_paths(paths, first);
    let more = total.saturating_sub(paths.len());
    Some(Hits { paths, more, everything: true, indexed: true })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_menu_shortcuts_and_exes_rank_ahead_of_os_stores() {
        let p = rank_paths(
            [
                r"C:\Users\Docs\editor".into(),
                r"C:\Users\Docs\My Editor.lnk".into(),
                r"C:\ProgramData\Start Menu\Programs\My Editor.lnk".into(),
            ],
            Some("editor"),
        );
        assert_eq!(p[0], r"C:\ProgramData\Start Menu\Programs\My Editor.lnk");
        assert_eq!(p[1], r"C:\Users\Docs\My Editor.lnk");
        assert_eq!(p[2], r"C:\Users\Docs\editor");
        let p = rank_paths([r"C:\Users\Docs\tool.txt".into(), r"C:\tooling.exe".into()], Some("tool"));
        assert_eq!(p[0], r"C:\tooling.exe");
        let p = rank_paths([r"C:\WinSxS\hosts".into(), r"C:\Users\Docs\hosts".into()], Some("hosts"));
        assert_eq!(p, vec![r"C:\Users\Docs\hosts", r"C:\WinSxS\hosts"]);
    }

    #[test]
    fn take_lines_drops_the_overflow_marker_and_counts_it() {
        let mut paths = Vec::new();
        let more = take_lines("C:\\a.txt\n... 12 more\nC:\\b.txt\n", &mut paths);
        assert_eq!(paths, vec![r"C:\a.txt", r"C:\b.txt"]);
        assert_eq!(more, 12);
        let mut paths = Vec::new();
        assert_eq!(take_lines("\n... not-a-number more\n", &mut paths), 0);
        assert!(paths.is_empty());
    }

    #[test]
    fn everything_ipc_is_skipped_when_the_client_is_not_running() {
        // CI has no Everything: this must stay green. A machine with the tray/UI client running does a real query.
        if everything_ipc::IpcWindow::new().is_none() {
            assert!(everything("*.exe").is_none(), "no IPC window means no Everything query");
            return;
        }
        let hits = everything("*.exe").expect("a running Everything UI should answer a query");
        assert!(hits.everything && hits.indexed);
    }
}
