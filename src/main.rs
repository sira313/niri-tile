use clap::Parser;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::signal;

// ---------------------------------------------------------------------------------------------------------------------
// %% Args Parser

#[derive(Parser, Debug)]
#[command(author, version, about = "Rust port of niri auto-tiler script")]
struct Args {
    #[arg(short = 'n', default_value_t = 3, help = "Number of windows handled with auto-tiling")]
    n: usize,

    #[arg(short = 'd', long = "delay", default_value_t = 0, help = "Number of milliseconds to delay before listening")]
    delay: u64,

    #[arg(short = 'x', default_value_t = true, action = clap::ArgAction::SetFalse, help = "Auto-maximize first window opened on a workspace")]
    maximize_solos: bool,

    #[arg(short = 'c', default_value_t = true, action = clap::ArgAction::SetFalse, help = "Collapse solo maximized window when opening a second window")]
    collapse_solos_on_open: bool,

    #[arg(long = "xc", default_value_t = true, action = clap::ArgAction::SetFalse, help = "When closing windows, if one window remains, auto-maximize it")]
    maximize_solo_on_close: bool,

    #[arg(short = 'm', default_value_t = false, action = clap::ArgAction::SetTrue, help = "Apply tiling logic to windows that are moved into other workspaces")]
    apply_on_move: bool,

    #[arg(short = 'e', long = "maximize_to_edges", action = clap::ArgAction::SetTrue, help = "Use maximize-to-edges instead of maximize-column")]
    maximize_to_edges: bool,

    #[arg(long = "dn", action = clap::ArgAction::SetTrue, help = "Enable event name printing, for debugging")]
    debug_names: bool,

    #[arg(long = "dd", action = clap::ArgAction::SetTrue, help = "Enable event data printing, for debugging")]
    debug_data: bool,

    #[arg(long = "iw", help = "Ignore workspace with this id (can be specified multiple times)")]
    ignored_workspace_ids: Vec<i64>,
}

// ---------------------------------------------------------------------------------------------------------------------
// %% Structs & Data Types

#[derive(Debug, Default, Clone)]
struct FocusState {
    workspace_id: Option<i64>,
    window_id: Option<i64>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct WindowLayout {
    pos_in_scrolling_layout: Option<(i32, i32)>,
    window_size: (f64, f64),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct WindowInfo {
    id: i64,
    workspace_id: Option<i64>,
    is_focused: bool,
    is_floating: bool,
    layout: Option<WindowLayout>,
    #[serde(default)]
    is_maximized: bool,
    #[serde(default)]
    col_idx: Option<i32>,
    #[serde(default)]
    row_idx: Option<i32>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct WorkspaceInfo {
    id: i64,
    is_focused: bool,
    output: Option<String>,
}

// ---------------------------------------------------------------------------------------------------------------------
// %% Helper Functions

fn get_niri_socket_path() -> Result<PathBuf, Box<dyn Error>> {
    let path_str = env::var("NIRI_SOCKET")
        .map_err(|_| "Environment variable NIRI_SOCKET tidak ditemukan. Apakah Niri sedang berjalan?")?;
    Ok(PathBuf::from(path_str))
}

fn augment_window_data(
    win: &mut WindowInfo,
    workspaces: &HashMap<i64, WorkspaceInfo>,
    output_width_lut: &HashMap<String, f64>,
) {
    if let Some(ref layout) = win.layout {
        if let Some(pos) = layout.pos_in_scrolling_layout {
            win.col_idx = Some(pos.0);
            win.row_idx = Some(pos.1);
        }
        
        if let Some(wspace_id) = win.workspace_id {
            if let Some(wspace) = workspaces.get(&wspace_id) {
                if let Some(ref output_name) = wspace.output {
                    if let Some(&out_width) = output_width_lut.get(output_name) {
                        let win_width = layout.window_size.0;
                        win.is_maximized = (win_width / out_width) > 0.8;
                    }
                }
            }
        }
    }
}

async fn send_action(stream: &mut UnixStream, action_name: &str, args: Value) -> Result<(), Box<dyn Error>> {
    let payload = json!({
        "Action": {
            action_name: args
        }
    });
    let mut msg = serde_json::to_string(&payload)?;
    msg.push('\n');
    stream.write_all(msg.as_bytes()).await?;
    
    // Baca response (flush buffer dari socket)
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    Ok(())
}

async fn toggle_window_maximization(
    stream: &mut UnixStream,
    target_id: i64,
    focused_id: Option<i64>,
    use_edges: bool,
) -> Result<(), Box<dyn Error>> {
    let act = if use_edges { "MaximizeWindowToEdges" } else { "MaximizeColumn" };
    
    if Some(target_id) == focused_id {
        send_action(stream, act, json!({})).await?;
    } else {
        send_action(stream, "FocusWindow", json!({"id": target_id})).await?;
        send_action(stream, act, json!({})).await?;
        if let Some(f_id) = focused_id {
            send_action(stream, "FocusWindow", json!({"id": f_id})).await?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------------
// %% Main Runtime

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();

    if args.delay > 0 {
        tokio::time::sleep(Duration::from_millis(args.delay)).await;
    }

    let socket_path = get_niri_socket_path()?;
    
    // Sinkronisasi info awal (Outputs)
    let mut init_stream = UnixStream::connect(&socket_path).await?;
    init_stream.write_all(b"\"Outputs\"\n").await?;
    let mut init_reader = BufReader::new(init_stream);
    let mut reply = String::new();
    init_reader.read_line(&mut reply).await?;
    
    let outputs_json: Value = serde_json::from_str(&reply)?;
    let mut output_width_lut = HashMap::new();
    
    if let Some(outputs) = outputs_json.get("Ok").and_then(|o| o.get("Outputs")) {
        if let Some(obj) = outputs.as_object() {
            for (key, val) in obj {
                if let Some(logical) = val.get("logical") {
                    if let Some(width) = logical.get("width").and_then(|w| w.as_f64()) {
                        output_width_lut.insert(key.clone(), width);
                    }
                }
            }
        }
    }

    // Koneksi utama untuk Event Stream dan Actions
    let mut stream = UnixStream::connect(&socket_path).await?;
    let (reader, writer) = UnixStream::connect(&socket_path).await?.into_split();
    
    // Ambil EventStream
    let mut writer_buf = writer;
    writer_buf.write_all(b"\"EventStream\"\n").await?;
    
    let mut event_reader = BufReader::new(reader);
    let mut line = String::new();
    
    let mut focus_state = FocusState::default();
    let mut win_state: HashMap<i64, WindowInfo> = HashMap::new();
    let mut wspace_state: HashMap<i64, WorkspaceInfo> = HashMap::new();
    
    let start_time = tokio::time::Instant::now();
    let mut last_debug_print = tokio::time::Instant::now();

    println!("Niri Autotiler (Rust) running...");

    loop {
        line.clear();
        tokio::select! {
            res = event_reader.read_line(&mut line) => {
                if res? == 0 { break; } // Socket closed
                
                let evt_json: Value = match serde_json::from_str(&line) {
                    Ok(j) => j,
                    Err(_) => continue,
                };

                let evt_name = match evt_json.as_object().and_then(|o| o.keys().next()) {
                    Some(k) => k.clone(),
                    None => continue,
                };
                
                let evt_data = &evt_json[&evt_name];

                if args.debug_names || args.debug_data {
                    if last_debug_print.elapsed().as_millis() > 250 {
                        println!("\nTime elapsed (sec): {}", start_time.elapsed().as_secs());
                        last_debug_print = tokio::time::Instant::now();
                    }
                    if args.debug_names { println!("{}", evt_name); }
                    if args.debug_data { println!("{}", evt_data); }
                }

                let mut closed_window_data: Option<WindowInfo> = None;
                let mut newest_window_data: Option<WindowInfo> = None;

                match evt_name.as_str() {
                    "WorkspacesChanged" => {
                        if let Some(workspaces) = evt_data.get("workspaces").and_then(|w| w.as_array()) {
                            for ws_val in workspaces {
                                if let Ok(ws) = serde_json::from_value::<WorkspaceInfo>(ws_val.clone()) {
                                    if ws.is_focused {
                                        focus_state.workspace_id = Some(ws.id);
                                    }
                                    wspace_state.insert(ws.id, ws);
                                }
                            }
                        }
                    }
                    "WorkspaceActivated" => {
                        if evt_data["focused"].as_bool().unwrap_or(false) {
                            if let Some(id) = evt_data["id"].as_i64() {
                                focus_state.workspace_id = Some(id);
                            }
                        }
                    }
                    "WindowsChanged" => {
                        if let Some(windows) = evt_data.get("windows").and_then(|w| w.as_array()) {
                            for win_val in windows {
                                if let Ok(mut win) = serde_json::from_value::<WindowInfo>(win_val.clone()) {
                                    augment_window_data(&mut win, &wspace_state, &output_width_lut);
                                    if win.is_focused {
                                        focus_state.window_id = Some(win.id);
                                    }
                                    win_state.insert(win.id, win);
                                }
                            }
                        }
                    }
                    "WindowOpenedOrChanged" => {
                        if let Some(win_val) = evt_data.get("window") {
                            if let Ok(mut win) = serde_json::from_value::<WindowInfo>(win_val.clone()) {
                                let win_id = win.id;
                                let is_new_window = !win_state.contains_key(&win_id);
                                let mut is_moved_window = false;
                                
                                if !is_new_window {
                                    if let Some(old_win) = win_state.get(&win_id) {
                                        is_moved_window = old_win.workspace_id != win.workspace_id;
                                    }
                                }

                                if win.is_focused {
                                    focus_state.window_id = Some(win_id);
                                }

                                augment_window_data(&mut win, &wspace_state, &output_width_lut);
                                win_state.insert(win_id, win.clone());

                                if is_new_window || (is_moved_window && args.apply_on_move) {
                                    newest_window_data = Some(win);
                                }
                            }
                        }
                    }
                    "WindowClosed" => {
                        if let Some(id) = evt_data["id"].as_i64() {
                            closed_window_data = win_state.remove(&id);
                        }
                    }
                    "WindowFocusChanged" => {
                        if let Some(id) = evt_data["id"].as_i64() {
                            focus_state.window_id = Some(id);
                        }
                    }
                    "WindowLayoutsChanged" => {
                        if let Some(changes) = evt_data.get("changes").and_then(|c| c.as_array()) {
                            for change in changes {
                                if let (Some(id), Some(layout_val)) = (change[0].as_i64(), change.get(1)) {
                                    if let Some(win) = win_state.get_mut(&id) {
                                        if let Ok(layout) = serde_json::from_value::<WindowLayout>(layout_val.clone()) {
                                            win.layout = Some(layout);
                                            augment_window_data(win, &wspace_state, &output_width_lut);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }

                // Logic: Handle Maximize-on-Close
                if let Some(closed_win) = closed_window_data {
                    if args.maximize_solo_on_close {
                        if let Some(ws_id) = closed_win.workspace_id {
                            // Perbaikan: Hanya collect ID dan status maximized (tipe data primitif / Copy)
                            let curr_wins: Vec<(i64, bool)> = win_state.values()
                                .filter(|w| w.workspace_id == Some(ws_id) && !w.is_floating)
                                .map(|w| (w.id, w.is_maximized))
                                .collect();
                                
                            if curr_wins.len() == 1 {
                                let (solo_id, is_maximized) = curr_wins[0];
                                if !is_maximized {
                                    toggle_window_maximization(&mut stream, solo_id, focus_state.window_id, args.maximize_to_edges).await?;
                                    if let Some(w) = win_state.get_mut(&solo_id) { w.is_maximized = true; }
                                }
                            }
                        }
                    }
                }

                // Logic: Handle Window Creation
                if let Some(new_win) = newest_window_data {
                    if !new_win.is_maximized && !new_win.is_floating {
                        if let Some(ws_id) = new_win.workspace_id {
                            if args.ignored_workspace_ids.contains(&ws_id) {
                                continue;
                            }

                            // Perbaikan: Collect tuple berisi data dasar untuk memutuskan logika tanpa menahan referensi map
                            let curr_tile_wins: Vec<(i64, bool)> = win_state.values()
                                .filter(|w| w.workspace_id == Some(ws_id) && !w.is_floating)
                                .map(|w| (w.id, w.is_maximized))
                                .collect();
                                
                            let num_tile_wins = curr_tile_wins.len();

                            if num_tile_wins > 0 && num_tile_wins <= args.n {
                                if args.maximize_solos && num_tile_wins == 1 {
                                    let (solo_id, is_maximized) = curr_tile_wins[0];
                                    if !is_maximized {
                                        toggle_window_maximization(&mut stream, solo_id, focus_state.window_id, args.maximize_to_edges).await?;
                                        if let Some(w) = win_state.get_mut(&solo_id) { w.is_maximized = true; }
                                    }
                                }

                                let max_wins: Vec<i64> = curr_tile_wins.iter()
                                    .filter(|&&(_, is_max)| is_max)
                                    .map(|&(id, _)| id)
                                    .collect();

                                if args.collapse_solos_on_open && max_wins.len() == 1 && num_tile_wins == 2 {
                                    let solo_max_id = max_wins[0];
                                    toggle_window_maximization(&mut stream, solo_max_id, focus_state.window_id, args.maximize_to_edges).await?;
                                    if let Some(w) = win_state.get_mut(&solo_max_id) { w.is_maximized = false; }
                                }
                            }
                        }
                    }
                }
            }
            _ = signal::ctrl_c() => {
                break;
            }
        }
    }

    println!("\nClosed niri IPC connection");
    Ok(())
}