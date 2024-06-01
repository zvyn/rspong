use axum::Form;
use axum::{
    extract::State,
    http::header,
    response::{
        sse::{Event, KeepAlive, Sse},
        Html,
    },
    routing::{get, post},
    Router,
};
use futures_util::stream::Stream;
use minijinja::{context, Environment};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::sync::{mpsc, Notify, RwLock};
use tokio::time::sleep;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_stream::wrappers::BroadcastStream;

#[derive(Clone, Serialize)]
struct Bat {
    up_key: String,
    down_key: String,
    position: f32,
    height: f32,
    score: u8,
}

#[derive(Clone, Serialize)]
struct Ball {
    position: (f32, f32),
    velocity: (f32, f32),
}

#[derive(Clone, Serialize)]
struct GameState {
    left: Bat,
    right: Bat,
    ball: Ball,
    is_running: bool,
}

#[derive(Clone)]
struct AppState {
    game: Arc<RwLock<GameState>>,
    templates: Environment<'static>,
    update_tx: broadcast::Sender<Event>,
    renderer: mpsc::Sender<Renderable>,
    wake_up: Arc<Notify>,
}

#[derive(Deserialize)]
struct KeyPress {
    last_key: String,
}

#[derive(Deserialize)]
struct MousePosition {
    x: f32,
    y: f32,
}

enum Renderable {
    Scoreboard,
    BatLeft,
    BatRight,
    Ball,
}

fn get_initial_state(render_tx: mpsc::Sender<Renderable>) -> AppState {
    let (tx, _) = broadcast::channel(50);
    AppState {
        game: Arc::new(RwLock::new(GameState {
            left: Bat {
                up_key: "w".to_string(),
                down_key: "s".to_string(),
                position: 40.0,
                score: 0,
                height: 20.,
            },
            right: Bat {
                up_key: "o".to_string(),
                down_key: "l".to_string(),
                position: 20.0,
                score: 0,
                height: 20.,
            },
            ball: Ball {
                position: (23.0, 42.0),
                velocity: (1.5, 0.5),
            },
            is_running: false,
        })),
        templates: create_template_env(),
        update_tx: tx,
        renderer: render_tx,
        wake_up: Arc::new(Notify::new()),
    }
}

#[tokio::main]
async fn main() {
    let (render_tx, render_rx) = mpsc::channel(50);
    let state = get_initial_state(render_tx);
    tokio::spawn(game_loop(state.clone()));
    tokio::spawn(render(state.clone(), render_rx));

    let app = Router::new()
        // Game views:
        .route("/", get(game_page))
        .route("/keypress", post(keypress))
        .route("/click", post(click))
        .route("/game-sse", get(sse_handler))
        .with_state(state)
        // Bake static files into binary:
        .route(
            "/scripts.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript")],
                    concat!(
                        include_str!("../static/htmx.min.js"),
                        include_str!("../static/sse.js")
                    ),
                )
            }),
        )
        .route(
            "/background.svg",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "image/svg+xml")],
                    include_str!("../static/bg.svg"),
                )
            }),
        )
        .route(
            "/favicon.svg",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "image/svg+xml")],
                    include_str!("../static/favicon.svg"),
                )
            }),
        );

    let listener = tokio::net::TcpListener::bind("[::1]:3000").await.unwrap();
    let addr = listener.local_addr().unwrap();
    println!("Listening on http://{addr}");
    axum::serve(listener, app).await.unwrap()
}

async fn render(state: AppState, mut render_rx: mpsc::Receiver<Renderable>) {
    let ball_template = state.templates.get_template("ball").unwrap();
    while let Some(renderable) = render_rx.recv().await {
        match renderable {
            Renderable::Scoreboard => {
                render_scoreboard(&state).await;
            }
            Renderable::Ball => {
                let game = state.game.read().await;
                let _ = state.update_tx.send(
                    Event::default().event("ball").data(
                        ball_template
                            .render(context! {game => *game })
                            .expect("ball renders"),
                    ),
                );
            }
            Renderable::BatLeft => {
                render_bat(&state, "bat_left").await;
            }
            Renderable::BatRight => {
                render_bat(&state, "bat_right").await;
            }
        };
    }
}

fn create_template_env() -> Environment<'static> {
    let mut env = Environment::new();
    env.add_template(
        "ball",
        "<div class=ball style=\"left: {{game.ball.position[0]}}%; top: {{game.ball.position[1]}}%;\"></div>"
    ).expect("ball template compiled");
    env.add_template(
        "bat_left",
        "<div id=\"bat_left\" class=bat style=\"top: {{game.left.position}}%; height: {{game.left.height}}vh;\"></div>",
    ).expect("bat left template compiled");
    env.add_template(
        "bat_right",
        "<div id=\"bat_right\" class=bat style=\"top: {{game.right.position}}%; height: {{game.right.height}}vh;\"></div>",
    ).expect("bat right template");
    env.add_template("scoreboard", include_str!("../templates/score.jinja2"))
        .expect("scoreboard template compiled");
    env.add_template("forkme", include_str!("../templates/forkme.jinja2"))
        .expect("forkme template compiled");
    env.add_template("game", include_str!("../templates/game.jinja2"))
        .expect("game template compiled");
    env
}

async fn game_loop(state: AppState) {
    loop {
        state.wake_up.notified().await;
        while {
            let game = state.game.read().await;
            game.is_running && state.update_tx.receiver_count() > 0
        } {
            update_ball_position(&state).await;
            sleep(Duration::from_millis(32)).await; // ~ 30Hz
        }
        state.game.write().await.is_running = false;
    }
}

async fn game_page(State(state): State<AppState>) -> Html<String> {
    let tmpl = state.templates.get_template("game").unwrap();
    Html(
        tmpl.render(context! {
            game => *state.game.read().await,
            players => state.update_tx.receiver_count(),
        })
        .expect("game renders"),
    )
}

async fn keypress(State(state): State<AppState>, Form(input): Form<KeyPress>) -> () {
    let mut g = state.game.write().await;
    let offset = 5.;

    if input.last_key.as_str() == "p" {
        g.is_running = !g.is_running;
        state.wake_up.notify_one();
        state.renderer.send(Renderable::Scoreboard).await.unwrap();
    } else if g.is_running {
        if input.last_key == g.left.up_key {
            move_bat(&mut g.left, offset * -1.);
            state.renderer.send(Renderable::BatLeft).await.unwrap();
        } else if input.last_key == g.left.down_key {
            move_bat(&mut g.left, offset);
            state.renderer.send(Renderable::BatLeft).await.unwrap();
        } else if input.last_key == g.right.up_key {
            move_bat(&mut g.right, offset * -1.);
            state.renderer.send(Renderable::BatRight).await.unwrap();
        } else if input.last_key == g.right.down_key {
            move_bat(&mut g.right, offset);
            state.renderer.send(Renderable::BatRight).await.unwrap();
        }
    };
}

async fn click(State(state): State<AppState>, Form(input): Form<MousePosition>) -> () {
    let mut g = state.game.write().await;
    if !g.is_running {
        g.is_running = true;
        state.wake_up.notify_one();
        state.renderer.send(Renderable::Scoreboard).await.unwrap();
    } else {
        let (bat, renderable) = {
            if input.x < 0.5 {
                (&mut g.left, Renderable::BatLeft)
            } else {
                (&mut g.right, Renderable::BatRight)
            }
        };
        let step = bat.height / 2.;
        if ((input.y * 100.) - (bat.height / 2.)) < bat.position {
            bat.position = (bat.position - step).max(1.)
        } else {
            bat.position = (bat.position + step).min(100.)
        }
        state.renderer.send(renderable).await.unwrap();
    };
}

async fn sse_handler(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, BroadcastStreamRecvError>>> {
    let stream = BroadcastStream::new(state.update_tx.subscribe());
    state.renderer.send(Renderable::Scoreboard).await.unwrap();
    Sse::new(stream).keep_alive(KeepAlive::default())
}

fn move_bat(b: &mut Bat, offset: f32) {
    b.position += offset;
    if offset > 0. && b.position > (100. - b.height) {
        b.position = 100. - b.height;
    } else if offset < 0. && b.position < 0. {
        b.position = 0.;
    }
}

async fn render_bat(state: &AppState, template_name: &str) {
    let tmpl = state.templates.get_template(template_name).unwrap();
    let _ = state.update_tx.send(
        Event::default().event(template_name.to_string()).data(
            tmpl.render(context! {
                game => *state.game.read().await
            })
            .expect("bat renders"),
        ),
    );
}

async fn render_scoreboard(state: &AppState) {
    let tmpl = state.templates.get_template("scoreboard").unwrap();
    let _ = state.update_tx.send(
        Event::default().event("scoreboard").data(
            tmpl.render(context! {
                game => *state.game.read().await,
                players => state.update_tx.receiver_count(),
            })
            .expect("scoreboard renders"),
        ),
    );
}

async fn update_ball_position(state: &AppState) {
    let mut g = state.game.write().await;
    let (x, y) = g.ball.position;
    g.ball.position = (x + g.ball.velocity.0, y + g.ball.velocity.1);
    if g.ball.position.0 <= 1. {
        g.ball.position = (1., g.ball.position.1);
        g.ball.velocity = (g.ball.velocity.0 * -1., g.ball.velocity.1);
        if !(g.ball.position.1 > g.left.position
            && g.ball.position.1 < g.left.position + g.left.height)
        {
            g.left.score += 1;
            g.left.height *= 1.1;
        } else {
            g.left.height *= 0.9;
        }
        state.renderer.send(Renderable::BatLeft).await.unwrap();
        state.renderer.send(Renderable::Scoreboard).await.unwrap();
    } else if g.ball.position.0 >= 99. {
        g.ball.position = (99., g.ball.position.1);
        g.ball.velocity = (g.ball.velocity.0 * -1., g.ball.velocity.1);
        if !(g.ball.position.1 > g.right.position
            && g.ball.position.1 < g.right.position + g.right.height)
        {
            g.right.score += 1;
            g.right.height *= 1.1;
        } else {
            g.right.height *= 0.9;
        }
        state.renderer.send(Renderable::BatRight).await.unwrap();
        state.renderer.send(Renderable::Scoreboard).await.unwrap();
    }
    if g.ball.position.1 <= 0. {
        g.ball.position = (g.ball.position.0, 0.);
        g.ball.velocity = (g.ball.velocity.0, g.ball.velocity.1 * -1.);
        state.renderer.send(Renderable::Scoreboard).await.unwrap();
    } else if g.ball.position.1 >= 99. {
        g.ball.position = (g.ball.position.0, 99.);
        g.ball.velocity = (g.ball.velocity.0, g.ball.velocity.1 * -1.);
        state.renderer.send(Renderable::Scoreboard).await.unwrap();
    }
    state.renderer.send(Renderable::Ball).await.unwrap();
}
