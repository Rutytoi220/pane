//! End-to-end integration test driving the real `pane-host` binary over
//! stdin/stdout pipes: handshake, ping, directory listing, file download, and
//! live PTY terminal state-diff streaming.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use pane_proto::{ClientMsg, HostMsg, PROTOCOL_VERSION, Screen, read_frame, write_frame};

#[test]
fn end_to_end_stdio_session() {
    let bin = env!("CARGO_BIN_EXE_pane-host");
    let mut child = Command::new(bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn pane-host");

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();

    // 1. Handshake + Ping.
    write_frame(&mut stdin, &ClientMsg::Hello { protocol_version: PROTOCOL_VERSION }).unwrap();
    write_frame(&mut stdin, &ClientMsg::Ping { timestamp_micros: 42_000 }).unwrap();
    stdin.flush().unwrap();

    let (welcome, _): (HostMsg, usize) = read_frame(&mut stdout).unwrap().unwrap();
    match welcome {
        HostMsg::Welcome { protocol_version, hostname, cpu_cores, .. } => {
            assert_eq!(protocol_version, PROTOCOL_VERSION);
            assert!(!hostname.is_empty());
            assert!(cpu_cores >= 1);
        }
        other => panic!("expected Welcome, got {other:?}"),
    }

    let (pong, _): (HostMsg, usize) = read_frame(&mut stdout).unwrap().unwrap();
    assert_eq!(pong, HostMsg::Pong { timestamp_micros: 42_000 });

    // 2. Directory listing + chunked file download.
    let tmp = std::env::temp_dir().join(format!("pane-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let payload: Vec<u8> = b"pane retained-mode state diff payload\n".repeat(4000);
    let sample_file = tmp.join("sample.txt");
    std::fs::write(&sample_file, &payload).unwrap();

    write_frame(&mut stdin, &ClientMsg::ListDir { req_id: 1, path: tmp.to_string_lossy().into() }).unwrap();
    write_frame(&mut stdin, &ClientMsg::DownloadFile { req_id: 2, path: sample_file.to_string_lossy().into() })
        .unwrap();
    write_frame(&mut stdin, &ClientMsg::ReadTextFile { req_id: 3, path: sample_file.to_string_lossy().into() })
        .unwrap();
    write_frame(&mut stdin, &ClientMsg::ListProcesses { req_id: 4 }).unwrap();
    // 3. Open a real PTY terminal and run a command.
    write_frame(&mut stdin, &ClientMsg::OpenTerminal { term_id: 1, cols: 80, rows: 24 }).unwrap();
    write_frame(&mut stdin, &ClientMsg::TermInput { term_id: 1, data: b"echo PANE_E2E_OK\r".to_vec() }).unwrap();
    stdin.flush().unwrap();

    let mut saw_listing = false;
    let mut downloaded = Vec::new();
    let mut download_done = false;
    let mut saw_preview = false;
    let mut saw_processes = false;
    let mut screen = Screen::new(80, 24);
    let mut saw_term_output = false;

    let deadline = Instant::now() + Duration::from_secs(10);
    while !(saw_listing && download_done && saw_preview && saw_processes && saw_term_output) {
        assert!(Instant::now() < deadline, "timed out waiting for host messages");
        let (msg, _): (HostMsg, usize) = read_frame(&mut stdout).unwrap().expect("unexpected EOF");
        match msg {
            HostMsg::DirListing { req_id: 1, entries, .. } => {
                assert!(entries.iter().any(|e| e.name == "sample.txt" && !e.is_dir));
                saw_listing = true;
            }
            HostMsg::FileChunk { req_id: 2, data, eof, .. } => {
                downloaded.extend_from_slice(&data);
                if eof {
                    download_done = true;
                }
            }
            HostMsg::TextFileContent { req_id: 3, content, truncated, .. } => {
                assert!(!truncated);
                assert!(content.starts_with("pane retained-mode state diff payload"));
                saw_preview = true;
            }
            HostMsg::ProcessList { req_id: 4, processes } => {
                assert!(!processes.is_empty());
                saw_processes = true;
            }
            HostMsg::TermDiff { term_id: 1, screen_cols, screen_rows, rows, .. } => {
                screen.apply(screen_cols, screen_rows, rows);
                if screen.text().contains("PANE_E2E_OK") {
                    saw_term_output = true;
                }
            }
            HostMsg::TelemetryUpdate { .. } | HostMsg::TermTitle { .. } => {}
            other => panic!("unexpected host message: {other:?}"),
        }
    }

    assert_eq!(downloaded, payload);

    // Clean shutdown: close terminal and EOF stdin.
    write_frame(&mut stdin, &ClientMsg::CloseTerminal { term_id: 1 }).unwrap();
    stdin.flush().unwrap();
    drop(stdin);

    let status = child.wait().unwrap();
    assert!(status.success(), "pane-host should exit cleanly on stdin EOF: {status}");
    let _ = std::fs::remove_dir_all(&tmp);
}
