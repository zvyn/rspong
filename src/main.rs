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
use std::{cmp, i32};
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
    position: u16,
    height: u16,
    score: u16,
}

#[derive(Clone, Serialize)]
struct Ball {
    position: (u16, u16),
    velocity: (i16, i16),
}

#[derive(Clone, Serialize)]
struct GameState {
    left: Bat,
    right: Bat,
    ball: Ball,
    is_running: bool,
    is_lost: bool,
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

enum Direction {
    Up,
    Down,
}


impl Default for Ball {
    fn default() -> Self {
        Self {
            position: (230, 420),
            velocity: (15, 5),
        }
    }
}


impl Bat {
    fn default_left() -> Self {
        Self {
            up_key: "w".to_string(),
            down_key: "s".to_string(),
            position: 600,
            score: 0,
            height: 200,
        }
    }

    fn default_right() -> Self {
        Self {
            up_key: "o".to_string(),
            down_key: "l".to_string(),
            position: 600,
            score: 0,
            height: 200,
        }
    }

    fn score_up(&mut self) {
        self.score += 1;
        self.height = cmp::max(10, self.height - self.height / 10);
    }
}


impl GameState {
    fn reset(&mut self) {
        self.left = Bat::default_left();
        self.right = Bat::default_right();
        self.ball = Ball::default();
        self.is_running = false;
        self.is_lost = false;
    }
}


impl Default for GameState {
    fn default() -> Self {
        Self {
            left: Bat::default_left(),
            right: Bat::default_right(),
            ball: Ball::default(),
            is_running: false,
            is_lost: false,
        }
    }
}


fn get_initial_state(render_tx: mpsc::Sender<Renderable>) -> AppState {
    let (tx, _) = broadcast::channel(50);
    AppState {
        game: Arc::new(RwLock::new(GameState::default())),
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
        "<div class=ball style=\"left: {{game.ball.position[0] / 10 }}%; top: {{game.ball.position[1] / 10}}%;\"></div>"
    ).expect("ball template compiled");
    env.add_template(
        "bat_left",
        "<div id=\"bat_left\" class=bat style=\"top: {{game.left.position / 10}}%; height: {{game.left.height / 10}}vh;\"></div>",
    ).expect("bat left template compiled");
    env.add_template(
        "bat_right",
        "<div id=\"bat_right\" class=bat style=\"top: {{game.right.position / 10}}%; height: {{game.right.height / 10}}vh;\"></div>",
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
        {
            let mut game = state.game.write().await;
            if game.is_lost {
                game.reset();
                render_all(&state).await;
            }
        }
        while {
            let game = state.game.read().await;
            game.is_running && !game.is_lost && state.update_tx.receiver_count() > 0
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
    let offset = 50;

    if input.last_key.as_str() == "p" {
        g.is_running = !g.is_running;
        state.wake_up.notify_one();
        state.renderer.send(Renderable::Scoreboard).await.unwrap();
    } else if g.is_running {
        if input.last_key == g.left.up_key {
            move_bat(&mut g.left, offset, Direction::Up);
            state.renderer.send(Renderable::BatLeft).await.unwrap();
        } else if input.last_key == g.left.down_key {
            move_bat(&mut g.left, offset, Direction::Down);
            state.renderer.send(Renderable::BatLeft).await.unwrap();
        } else if input.last_key == g.right.up_key {
            move_bat(&mut g.right, offset, Direction::Up);
            state.renderer.send(Renderable::BatRight).await.unwrap();
        } else if input.last_key == g.right.down_key {
            move_bat(&mut g.right, offset, Direction::Down);
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
        let step = bat.height / 2;
        let y = (input.y * 1000.) as u16;
        if y < (bat.position + (bat.height / 2)) {
            bat.position = if step < bat.position {bat.position - step} else {1}
        } else {
            bat.position = (bat.position + step).min(1000)
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

fn move_bat(b: &mut Bat, offset: u16, direction: Direction) {
    match direction {
        Direction::Up => {
            b.position = if offset < b.position {b.position - offset} else {0};
        },
        Direction::Down => {
            b.position = cmp::min(1000 - b.height, b.position + offset);
        }
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
    g.ball.position = (
        ((g.ball.velocity.0 as i32) + (g.ball.position.0 as i32)) as u16,
        ((g.ball.velocity.1 as i32) + (g.ball.position.1 as i32)) as u16,
    );
    if g.ball.position.0 <= 10 {
        if g.ball.position.1 > g.left.position
            && g.ball.position.1 < g.left.position + g.left.height
        {
            g.ball.position = (10, g.ball.position.1);
            g.ball.velocity = (g.ball.velocity.0 * -1, g.ball.velocity.1);
            g.left.score_up();
            state.renderer.send(Renderable::BatLeft).await.unwrap();
            state.renderer.send(Renderable::Scoreboard).await.unwrap();
        } else {
            g.is_lost = true;
            render_all(state).await;
        }
    } else if g.ball.position.0 >= 990 {
        if g.ball.position.1 > g.right.position
            && g.ball.position.1 < g.right.position + g.right.height
        {
            g.ball.position = (990, g.ball.position.1);
            g.ball.velocity = (g.ball.velocity.0 * -1, g.ball.velocity.1);
            g.right.score_up();
            state.renderer.send(Renderable::BatRight).await.unwrap();
            state.renderer.send(Renderable::Scoreboard).await.unwrap();
        } else {
            g.is_lost = true;
            render_all(state).await;
        }
    }
    if g.ball.position.1 <= 0 {
        g.ball.position = (g.ball.position.0, 0);
        g.ball.velocity = (g.ball.velocity.0, g.ball.velocity.1 * -1);
        state.renderer.send(Renderable::Scoreboard).await.unwrap();
    } else if g.ball.position.1 >= 990 {
        g.ball.position = (g.ball.position.0, 990);
        g.ball.velocity = (g.ball.velocity.0, g.ball.velocity.1 * -1);
        state.renderer.send(Renderable::Scoreboard).await.unwrap();
    }
    state.renderer.send(Renderable::Ball).await.unwrap();
}


async fn render_all(state: &AppState) {
    state.renderer.send(Renderable::BatLeft).await.unwrap();
    state.renderer.send(Renderable::BatRight).await.unwrap();
    state.renderer.send(Renderable::Scoreboard).await.unwrap();
    state.renderer.send(Renderable::Ball).await.unwrap();
}
