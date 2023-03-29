use axum::{
    extract::{
        ws::{Message, WebSocket},
        State, WebSocketUpgrade,
    },
    response::{IntoResponse},
    routing::get,
    Router, Server,
};
use serde_json::json;
use tokio::sync::broadcast;
use rand::{Rng, rngs::StdRng, SeedableRng, seq::SliceRandom};
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use num_format::{Locale, WriteFormatted};

#[derive(Clone)]
struct AppState {
    tx: broadcast::Sender<Event>,
}

#[derive(Serialize, Clone)]
enum Event {
    CreateGame {
        game_id: usize,
    },
    UpdateGame {
        game_id: usize,
        board: [usize; 9],
    },
    GameEnd {
        game_id: usize,
        winner: usize,
        winning_line: [usize; 3],
    },
}

#[derive(Serialize)]
struct MessageOutput {
    event_name: String,
    event_data: Event,
}

#[tokio::main]
async fn main() {
    let (tx, _) = broadcast::channel::<Event>(100);

    tracing_subscriber::fmt::init();

    let app_state = AppState { tx: tx.clone() };

    let router = Router::new()
        .route("/realtime/ttt", get(realtime_ttt_get))
        .with_state(app_state.clone());

    let msg_countc = Arc::new(AtomicUsize::new(0));

    // Spawn a task to log the message rate every second
    let log_msg_countc = Arc::clone(&msg_countc);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
            let count = log_msg_countc.swap(0, Ordering::Relaxed);

            let mut formatted_create_count = String::new();
            formatted_create_count.write_formatted(&count, &Locale::en).unwrap();
            println!("[CREATED] {} messages in the last second", formatted_create_count);
        }
    });

    tokio::task::spawn(async move {
        let mut rng = StdRng::from_entropy();
        let mut game_id = 0;
    
        let mut interval = tokio::time::interval(std::time::Duration::from_micros(5_000)); // Run at 200 Hz
        loop {
            // Create a new game and send a create game event
            game_id += 1;
            let create_event = Event::CreateGame { game_id };
            msg_countc.fetch_add(1, Ordering::Relaxed);
            let _ = tx.send(create_event);
    
            let mut board: [usize; 9] = [0; 9];
            for _ in 0..9 {
                if false {
                    interval.tick().await;
                }
                
                let available_cells: Vec<usize> = board
                    .iter()
                    .enumerate()
                    .filter(|(_, &cell)| cell == 0)
                    .map(|(i, _)| i)
                    .collect();
            
                if let Some(&move_index) = available_cells.choose(&mut rng) {
                    board[move_index] = rng.gen_range(1..3);
            
                    let update_event = Event::UpdateGame { game_id, board };
                    msg_countc.fetch_add(1, Ordering::Relaxed);
                    let _ = tx.send(update_event);
            
                    // Check for a win condition or a draw
                    if let Some(winning_line) = get_winning_line(&board) {
                        let winner = board[winning_line[0]];
            
                        // Send the game end event
                        let end_event = Event::GameEnd { game_id, winner, winning_line };
                        msg_countc.fetch_add(1, Ordering::Relaxed);
                        let _ = tx.send(end_event);
                        break;
                    } else if board.iter().all(|&cell| cell != 0) {
                        // Send the game end event for a draw
                        let end_event = Event::GameEnd { game_id, winner: 0, winning_line: [0, 0, 0] };
                        msg_countc.fetch_add(1, Ordering::Relaxed);
                        let _ = tx.send(end_event);
                        break;
                    }

                    // Yield to give other tasks a chance to run
                    tokio::task::yield_now().await;
                } else {
                    // In the rare case when there are no available cells but no winner, consider it a draw
                    let end_event = Event::GameEnd { game_id, winner: 0, winning_line: [0, 0, 0] };
                    msg_countc.fetch_add(1, Ordering::Relaxed);
                    let _ = tx.send(end_event);
                    break;
                }
            }
        }
    });

        let server = Server::bind(&"0.0.0.0:7032".parse().unwrap()).serve(router.into_make_service());
        let addr = server.local_addr();
        println!("Listening on {addr}");

        server.await.unwrap();
    }

    #[axum::debug_handler]
    async fn realtime_ttt_get(
        ws: WebSocketUpgrade,
        State(state): State<AppState>,
        ) -> impl IntoResponse {
        ws.on_upgrade(|ws: WebSocket| async { realtime_ttt_stream(state, ws).await })
    }
    
    async fn realtime_ttt_stream(app_state: AppState, ws: WebSocket) {
        let mut rx = app_state.tx.subscribe();
        let ws = Arc::new(Mutex::new(ws));
    
        while let Ok(event) = rx.recv().await {
            let message_output = match &event {
                Event::CreateGame { game_id } => {
                    json!(["c", { "i": game_id }])
                }
                Event::UpdateGame { game_id, board } => {
                    json!(["u", { "i": game_id, "p": board }])
                }
                Event::GameEnd { game_id, winner, winning_line } => {
                    json!(["e", { "i": game_id, "w": winner, "wl": winning_line }])
                }
            };
    
            let message_text = Message::Text(serde_json::to_string(&message_output).unwrap());
    
            let mut ws_lock = ws.lock().await;
            if let Err(e) = ws_lock.send(message_text).await {
                eprint!("Error sending {:?}", e);
                break;
            }
        }
    }

fn get_winning_line(board: &[usize; 9]) -> Option<[usize; 3]> {
    // Check for a winner using the row, column, and diagonal indices
    let win_conditions = [
        [0, 1, 2],
        [3, 4, 5],
        [6, 7, 8],
        [0, 3, 6],
        [1, 4, 7],
        [2, 5, 8],
        [0, 4, 8],
        [2, 4, 6],
    ];

    win_conditions.iter().find_map(|&line| {
        if board[line[0]] != 0 && board[line[0]] == board[line[1]] && board[line[0]] == board[line[2]] {
            Some(line)
        } else {
            None
        }
    })
}
