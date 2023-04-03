use axum::{
    extract::{
        ws::{Message, WebSocket},
        State, WebSocketUpgrade,
    },
    response::IntoResponse,
    routing::get,
    Router,
};
use axum_server::AddrIncomingConfig;
use num_format::{Locale, WriteFormatted};
use rand::{
    rngs::StdRng,
    seq::{IteratorRandom, SliceRandom},
    Rng, SeedableRng,
};
use serde::Serialize;
use serde_json::json;
use std::sync::Arc;
use std::{
    net::SocketAddr,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
use tokio::sync::broadcast;

#[derive(Clone)]
struct AppState {
    tx: broadcast::Sender<Event>,
    conn_count: Arc<AtomicUsize>,
}

#[derive(Debug, Serialize, Clone)]
enum Event {
    CreateGame {
        id: usize,
    },
    UpdateGame {
        id: usize,
        new_position: [usize; 2],
    },
    GameEnd {
        id: usize,
        winner: usize,
        winning_line: [usize; 9],
    },
    Stats {
        connections: usize,
    },
}

#[derive(Serialize)]
struct MessageOutput {
    event_name: String,
    event_data: Event,
}

struct Board {
    id: usize,
    positions: [usize; 3600],
    winner: Option<usize>,
}

#[tokio::main]
async fn main() {
    let (tx, _) = broadcast::channel::<Event>(8);

    tracing_subscriber::fmt::init();

    let app_state = AppState {
        tx: tx.clone(),
        conn_count: Arc::new(AtomicUsize::new(0)),
    };

    let win_conditions = generate_win_conditions();

    let tps_count = Arc::new(AtomicUsize::new(0));
    log_stats(Arc::clone(&tps_count), Arc::clone(&app_state.conn_count));
    let conn_count_clone = Arc::clone(&app_state.conn_count);

    tokio::task::spawn(async move {
        let mut rng = StdRng::from_entropy();

        let mut tick_interval = tokio::time::interval(std::time::Duration::from_micros(5_000));

        const MAX_CONCURRENT_GAMES: usize = 2;
        const MAX_STORED_GAMES: usize = 10_000;
        let mut boards: Vec<Board> = vec![];
        let mut total_boards_created: usize = 0;
        loop {
            if boards.len() > MAX_STORED_GAMES {
                println!(
                    "FILTERED GAMES -- max stored games ({}) was reached",
                    MAX_STORED_GAMES
                );
                boards.retain(|board| board.winner.is_none());
            }

            // Create a new game and send a create game event
            if boards.iter().filter(|board| board.winner.is_none()).count() < MAX_CONCURRENT_GAMES {
                let id = total_boards_created;
                total_boards_created += 1;

                // Every 10 boards send a stats update
                if id % 10 == 0 {
                    match tx.send(Event::Stats { connections: conn_count_clone.load(Ordering::Relaxed) }) {
                        Ok(_) => (),
                        Err(_) => (),
                    }
                }

                match tx.send(Event::CreateGame { id }) {
                    Ok(_) => (),
                    Err(_) => (),
                }

                let board = Board {
                    id,
                    positions: [0; 3600],
                    winner: None,
                };
                boards.push(board);
            }

            // Choose a random unfinished game
            if let Some((_, ticking_game)) = boards
                .iter_mut()
                .enumerate()
                .filter(|(_, board)| board.winner.is_none())
                .choose(&mut rng)
            {
                let available_cells: Vec<usize> = ticking_game
                    .positions
                    .iter()
                    .enumerate()
                    .filter(|(_, &cell)| cell == 0)
                    .map(|(i, _)| i)
                    .collect();

                if let Some(&move_index) = available_cells.choose(&mut rng) {
                    ticking_game.positions[move_index] = rng.gen_range(1..3);

                    tps_count.fetch_add(1, Ordering::Relaxed);
                    match tx.send(Event::UpdateGame {
                        id: ticking_game.id,
                        new_position: [move_index, ticking_game.positions[move_index]],
                    }) {
                        Ok(_) => (),
                        Err(_) => (),
                    }

                    // Check for a win condition or a draw
                    if let Some(winning_line) = get_winning_line(&ticking_game.positions, &win_conditions) {
                        let winner = ticking_game.positions[winning_line[0]];
                        ticking_game.winner = Some(winner);

                        match tx.send(Event::GameEnd {
                            id: ticking_game.id,
                            winner,
                            winning_line,
                        }) {
                            Ok(_) => (),
                            Err(_) => (),
                        }
                    } else if ticking_game.positions.iter().all(|&cell| cell != 0) {
                        // Draw case
                        ticking_game.winner = Some(0);
                        match tx.send(Event::GameEnd {
                            id: ticking_game.id,
                            winner: 0,
                            winning_line: [0, 0, 0, 0, 0, 0, 0, 0, 0],
                        }) {
                            Ok(_) => (),
                            Err(_) => (),
                        }
                    }

                    // true = normal mode. false = fast boi mode
                    if false {
                        tick_interval.tick().await;
                    }
                    tokio::task::yield_now().await;
                }
            }
        }
    });

    let router = Router::new()
        .route("/realtime/ttt", get(realtime_ttt_get))
        .with_state(app_state.clone());

    let config = AddrIncomingConfig::new()
        .tcp_nodelay(true)
        .tcp_sleep_on_accept_errors(true)
        .tcp_keepalive(Some(Duration::from_secs(32)))
        .tcp_keepalive_interval(Some(Duration::from_secs(1)))
        .tcp_keepalive_retries(Some(1))
        .build();

    let addr = SocketAddr::from(([0, 0, 0, 0], 7032));
    println!("[TTT REALTIME] Listening on {}", addr);
    axum_server::bind(addr)
        .addr_incoming_config(config)
        .serve(router.into_make_service())
        .await
        .unwrap();
}

#[axum::debug_handler]
async fn realtime_ttt_get(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(|ws: WebSocket| async { realtime_ttt_stream(state, ws).await })
}

async fn realtime_ttt_stream(app_state: AppState, mut ws: WebSocket) {
    let mut rx = app_state.tx.subscribe();
    app_state.conn_count.fetch_add(1, Ordering::Relaxed);

    while let Ok(event) = rx.recv().await {
        let message_output = match &event {
            Event::CreateGame { id } => {
                json!(["c", { "i": id }])
            }
            Event::UpdateGame { id, new_position } => {
                json!(["u", { "i": id, "p": [new_position[0], new_position[1]] }])
            }
            Event::GameEnd {
                id,
                winner,
                winning_line,
            } => {
                json!(["e", { "i": id, "w": winner, "wl": winning_line }])
            }
            Event::Stats { connections } => {
                json!(["s", { "conn": connections }])
            }
        };

        let message_text = Message::Text(serde_json::to_string(&message_output).unwrap());

        if let Err(e) = ws.send(message_text).await {
            eprintln!("NET ERROR: {:?}", e);
            break; // Disconnect if we encounter an error (typically a client leaving)
        }
    }

    app_state.conn_count.fetch_min(1, Ordering::Relaxed);
}


fn get_winning_line(board: &[usize; 3600], win_conditions: &Vec<Vec<usize>>) -> Option<[usize; 9]> {
    

    win_conditions.iter().find_map(|line| {
        let mut consecutive = 1;
        for i in 1..line.len() {
            if board[line[i]] != 0 && board[line[i]] == board[line[i - 1]] {
                consecutive += 1;
            } else {
                consecutive = 1;
            }

            if consecutive == 9 {
                let mut winning_line = [0; 9];
                winning_line.copy_from_slice(&line[i - 8..=i]);
                return Some(winning_line);
            }
        }
        None
    })
}


fn generate_win_conditions() -> Vec<Vec<usize>> {
    let mut win_conditions = Vec::new();
    let size = 60;

    for row in 0..size {
        for col in 0..size {
            let mut hor_line = Vec::new();
            let mut ver_line = Vec::new();
            let mut diag_line1 = Vec::new();
            let mut diag_line2 = Vec::new();

            for i in 0..9 {
                if col + 8 < size {
                    hor_line.push(row * size + col + i);
                }

                if row + 8 < size {
                    ver_line.push((row + i) * size + col);
                }

                if col + 8 < size && row + 8 < size {
                    diag_line1.push((row + i) * size + col + i);
                }

                if col >= 8 && row + 8 < size {
                    diag_line2.push((row + i) * size + col - i);
                }
            }

            if !hor_line.is_empty() {
                win_conditions.push(hor_line);
            }
            if !ver_line.is_empty() {
                win_conditions.push(ver_line);
            }
            if !diag_line1.is_empty() {
                win_conditions.push(diag_line1);
            }
            if !diag_line2.is_empty() {
                win_conditions.push(diag_line2);
            }
        }
    }

    win_conditions
}



fn log_stats(tps_count: Arc<AtomicUsize>, conn_count: Arc<AtomicUsize>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
            let count = tps_count.swap(0, Ordering::Relaxed);

            let mut formatted_create_count = String::new();
            formatted_create_count
                .write_formatted(&count, &Locale::en)
                .unwrap();
            println!(
                "Processed {} TPS for {:?} connected client(s)",
                formatted_create_count,
                conn_count.clone()
            );
        }
    });
}
