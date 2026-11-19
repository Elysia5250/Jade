//! Event-driven console loop.
//!
//! The terminal is read on a blocking thread. The async loop waits on that
//! stream, daemon messages, and a timer. A frame timer is armed only while a
//! decrypt / in-progress animation is on screen.

use super::*;
use std::future::Future;

const FRAME_MS: u64 = 33;
const ASK_FRAME_MS: u64 = 250;
const SHELL_POLL: Duration = Duration::from_millis(200);

type ModelMsg = (String, Vec<envoy_agent::agent::context::ListedModel>);

struct WakePlan {
    after: Duration,
    draw: bool,
}

enum ConsoleWake {
    Term(Option<Event>),
    Notice(Option<UiNotice>),
    Agent(Option<AgentResp>),
    Login(Option<LoginEvent>),
    Model(std::result::Result<ModelMsg, oneshot::error::RecvError>),
    Timer { draw: bool },
}

enum Step {
    Quit,
    Stay { redraw: bool },
}

/// Idle consoles do not arm this. Decrypt and ask countdown do.
pub(crate) fn animation_tick(app: &App) -> Option<Duration> {
    if app.agent_running {
        Some(Duration::from_millis(FRAME_MS))
    } else if app.pending_ask.is_some() {
        Some(Duration::from_millis(ASK_FRAME_MS))
    } else {
        None
    }
}

pub(crate) async fn console_loop(
    app: &mut App,
    tui: &mut Tui,
    remote: &mut Remote<AnyDialer>,
) -> Result<()> {
    let (tx, mut term_rx) = mpsc::unbounded_channel();
    if std::thread::Builder::new()
        .name("envoy-tty".into())
        .spawn(move || tty_reader(tx))
        .is_err()
    {
        bail!("tty reader failed to start");
    }
    let mut redraw = false;
    loop {
        if inputs::pull_remote(app, remote).await {
            redraw = true;
        }
        if redraw {
            paint_now(tui, app)?;
            redraw = false;
            if inputs::remote_pending(app) {
                continue;
            }
        }
        match step(app, tui, remote, &mut term_rx).await? {
            Step::Quit => return Ok(()),
            Step::Stay { redraw: again } => redraw = again,
        }
    }
}

fn tty_reader(tx: mpsc::UnboundedSender<Event>) {
    loop {
        if tx.is_closed() {
            break;
        }
        match event::poll(Duration::from_millis(100)) {
            Ok(true) => match event::read() {
                Ok(ev) => {
                    if tx.send(ev).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            },
            Ok(false) => {}
            Err(_) => break,
        }
    }
}

fn paint_now(tui: &mut Tui, app: &mut App) -> Result<()> {
    let _ = execute!(stdout(), BeginSynchronizedUpdate);
    tui.terminal.draw(|frame| paint(frame, app))?;
    let _ = execute!(stdout(), EndSynchronizedUpdate);
    Ok(())
}

async fn step(
    app: &mut App,
    tui: &mut Tui,
    remote: &mut Remote<AnyDialer>,
    term_rx: &mut mpsc::UnboundedReceiver<Event>,
) -> Result<Step> {
    let plan = next_wake(app);
    let draw = plan.as_ref().is_some_and(|plan| plan.draw);
    let delay = plan.map(|plan| plan.after);
    let mut agent_rx = app.agent_stream.take();
    let mut login_rx = app.login_rx.take();
    let mut model_rx = app.models_rx.take();
    let wake = wait_console(
        term_rx,
        &mut app.notice_rx,
        &mut agent_rx,
        &mut login_rx,
        &mut model_rx,
        delay,
        draw,
    )
    .await;
    app.agent_stream = agent_rx;
    app.login_rx = login_rx;
    app.models_rx = model_rx;
    let mut redraw = false;
    if dispatch_wake(app, tui, remote, term_rx, wake, &mut redraw).await? {
        return Ok(Step::Quit);
    }
    if settle_background(app, remote).await {
        redraw = true;
    }
    Ok(Step::Stay { redraw })
}

async fn dispatch_wake(
    app: &mut App,
    tui: &mut Tui,
    remote: &mut Remote<AnyDialer>,
    term_rx: &mut mpsc::UnboundedReceiver<Event>,
    wake: ConsoleWake,
    redraw: &mut bool,
) -> Result<bool> {
    match wake {
        ConsoleWake::Term(None) => return Ok(true),
        ConsoleWake::Term(Some(ev)) => {
            if on_terminal(app, tui, remote, ev).await? {
                return Ok(true);
            }
            while let Ok(ev) = term_rx.try_recv() {
                if on_terminal(app, tui, remote, ev).await? {
                    return Ok(true);
                }
            }
            *redraw = true;
        }
        ConsoleWake::Notice(Some(notice)) => {
            app.apply_notice(notice);
            *redraw = true;
        }
        ConsoleWake::Notice(None) => {}
        ConsoleWake::Agent(message) => {
            app.drain_agent_batch(message);
            *redraw = true;
        }
        ConsoleWake::Login(None) => {
            app.login_rx = None;
        }
        ConsoleWake::Login(Some(ev)) => {
            app.drain_login_from(Some(ev));
            *redraw = true;
        }
        ConsoleWake::Model(result) => {
            match result {
                Ok((provider_id, fetched)) => app.apply_listed_models(provider_id, fetched),
                Err(_) => app.model_fetch_failed(),
            }
            *redraw = true;
        }
        ConsoleWake::Timer { draw } => *redraw = draw,
    }
    // The match applied the winning event. Anything else already queued is
    // drained here so one wake paints at most once.
    if app.drain_agent_events() || app.drain_ui_notices() || app.drain_login_events() {
        *redraw = true;
    }
    if app.drain_model_events() {
        *redraw = true;
    }
    Ok(false)
}

async fn wait_console(
    term_rx: &mut mpsc::UnboundedReceiver<Event>,
    notice_rx: &mut mpsc::UnboundedReceiver<UiNotice>,
    agent_rx: &mut Option<mpsc::UnboundedReceiver<AgentResp>>,
    login_rx: &mut Option<mpsc::UnboundedReceiver<LoginEvent>>,
    model_rx: &mut Option<oneshot::Receiver<ModelMsg>>,
    delay: Option<Duration>,
    draw: bool,
) -> ConsoleWake {
    let model_open = model_rx.is_some();
    tokio::select! {
        biased;
        ev = term_rx.recv() => ConsoleWake::Term(ev),
        notice = notice_rx.recv() => ConsoleWake::Notice(notice),
        message = wait_stream(agent_rx) => ConsoleWake::Agent(message),
        ev = wait_stream(login_rx) => ConsoleWake::Login(ev),
        result = wait_model(model_rx), if model_open => {
            *model_rx = None;
            ConsoleWake::Model(result)
        }
        _ = sleep_for(delay) => ConsoleWake::Timer { draw },
    }
}

async fn wait_stream<T>(slot: &mut Option<mpsc::UnboundedReceiver<T>>) -> Option<T> {
    match slot.as_mut() {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

async fn wait_model(
    slot: &mut Option<oneshot::Receiver<ModelMsg>>,
) -> std::result::Result<ModelMsg, oneshot::error::RecvError> {
    let Some(rx) = slot.as_mut() else {
        return std::future::pending().await;
    };
    std::future::poll_fn(|cx| std::pin::Pin::new(&mut *rx).poll(cx)).await
}

async fn sleep_for(delay: Option<Duration>) {
    match delay {
        Some(delay) => tokio::time::sleep(delay).await,
        None => std::future::pending().await,
    }
}

fn next_wake(app: &App) -> Option<WakePlan> {
    let mut best = None;
    if let Some(after) = animation_tick(app) {
        push_wake(&mut best, after, true);
    }
    if let Some((_, _, at)) = &app.toast {
        if at.elapsed() < Duration::from_secs(6) {
            let left = Duration::from_secs(6).saturating_sub(at.elapsed());
            push_wake(&mut best, left, true);
        }
    }
    let refresh_left = app.interval.saturating_sub(app.last_refresh.elapsed());
    push_wake(&mut best, refresh_left, false);
    if !app.shell_watches.is_empty() {
        let left = SHELL_POLL.saturating_sub(app.last_shell_poll.elapsed());
        push_wake(&mut best, left, false);
    }
    if app.welcome.is_none() && app.agent_stream.is_none() {
        let left = app.agent_retry_at.saturating_duration_since(Instant::now());
        push_wake(&mut best, left, false);
    }
    best
}

fn push_wake(best: &mut Option<WakePlan>, after: Duration, draw: bool) {
    match best {
        None => *best = Some(WakePlan { after, draw }),
        Some(plan) if after < plan.after => {
            *best = Some(WakePlan {
                after,
                draw: plan.draw || draw,
            })
        }
        Some(plan) if after == plan.after => plan.draw |= draw,
        _ => {}
    }
}

async fn settle_background(app: &mut App, remote: &Remote<AnyDialer>) -> bool {
    let mut dirty = false;
    if app.last_refresh.elapsed() >= app.interval {
        let before = job_fingerprint(app);
        let connected = app.connected;
        app.refresh(remote).await;
        dirty |= app.connected != connected || job_fingerprint(app) != before;
    }
    if shell_poll_due(app) {
        app.poll_shell_cards(remote).await;
        app.last_shell_poll = Instant::now();
        dirty = true;
    }
    if app.welcome.is_none() && app.agent_stream.is_none() && Instant::now() >= app.agent_retry_at
    {
        app.spawn_agent_attach();
        dirty = true;
    }
    dirty
}

fn shell_poll_due(app: &App) -> bool {
    !app.shell_watches.is_empty() && app.last_shell_poll.elapsed() >= SHELL_POLL
}

fn job_fingerprint(app: &App) -> Vec<(u64, u64, Option<u32>, JobState)> {
    app.jobs
        .iter()
        .map(|job| (job.id, job.log_bytes, job.pid, job.state.clone()))
        .collect()
}

async fn on_terminal(
    app: &mut App,
    tui: &mut Tui,
    remote: &mut Remote<AnyDialer>,
    ev: Event,
) -> Result<bool> {
    match ev {
        Event::Key(key) => {
            if key.kind != KeyEventKind::Press {
                return Ok(false);
            }
            app.selection = None;
            app.screen_lines.clear();
            let action = app.handle_key(key);
            on_action(app, tui, remote, action).await
        }
        Event::Paste(text) => {
            app.selection = None;
            app.screen_lines.clear();
            app.handle_paste(&text);
            Ok(false)
        }
        Event::Mouse(mouse) => {
            if let Some(Action::AttachJob(job)) = app.handle_mouse(mouse) {
                app.attach_job_card(remote, job).await;
            }
            Ok(false)
        }
        _ => Ok(false),
    }
}

async fn on_action(
    app: &mut App,
    tui: &mut Tui,
    remote: &mut Remote<AnyDialer>,
    action: Action,
) -> Result<bool> {
    match action {
        Action::Quit => {
            app.persist_session();
            Ok(true)
        }
        Action::None => Ok(false),
        Action::SubmitShell(req) => {
            app.submit_shell(remote, req).await;
            Ok(false)
        }
        Action::SubmitAgent(prompt) => {
            app.submit_agent(prompt);
            Ok(false)
        }
        Action::SubmitAgentFollowup(prompt) => {
            app.queue_followup(prompt);
            Ok(false)
        }
        Action::Cancel => {
            app.cancel_active(remote).await;
            Ok(false)
        }
        Action::CancelJob(id) => {
            app.cancel_job(remote, id).await;
            Ok(false)
        }
        Action::Refresh => {
            app.refresh(remote).await;
            Ok(false)
        }
        Action::Switch(target) => {
            app.switch_to(target);
            if app.target == "local" {
                daemon::ensure(&app.data_dir).await?;
            }
            *remote = Remote::new(dialer_for(
                &app.data_dir,
                &app.target,
                app.conduit.clone(),
            ));
            app.refresh(remote).await;
            Ok(false)
        }
        Action::AttachJob(job) => {
            app.attach_job_card(remote, job).await;
            Ok(false)
        }
        Action::ToggleMouseCapture => {
            let next = !tui.mouse_capture;
            tui.set_mouse_capture(next);
            app.mouse_capture = next;
            app.set_toast(
                if next {
                    "mouse capture: on (drag to OSC52 copy, click links)"
                } else {
                    "mouse capture: off (native terminal selection)"
                },
                false,
            );
            Ok(false)
        }
        Action::SetMouseCapture(on) => {
            tui.set_mouse_capture(on);
            app.mouse_capture = on;
            Ok(false)
        }
        Action::EditFile(path) => {
            if let Err(err) = edit_file::run(tui, &path) {
                app.set_toast(err, true);
            }
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let mut bytes = [0u8; 4];
        getrandom::fill(&mut bytes).unwrap();
        let dir = std::env::temp_dir().join(format!(
            "envoy-drv-{:02x}{:02x}{:02x}{:02x}",
            bytes[0], bytes[1], bytes[2], bytes[3]
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn idle_does_not_arm_a_frame_tick() {
        let dir = tmpdir();
        let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
        app.agent_running = false;
        app.pending_ask = None;
        app.toast = None;
        app.last_refresh = Instant::now();
        assert!(animation_tick(&app).is_none());
        let plan = next_wake(&app).expect("refresh still schedules background work");
        assert!(!plan.draw);
        app.agent_running = true;
        assert_eq!(animation_tick(&app), Some(Duration::from_millis(FRAME_MS)));
        assert!(next_wake(&app).is_some_and(|plan| plan.draw));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persist_hold_writes_once_when_released() {
        let dir = tmpdir();
        let mut app = App::new(dir.clone(), "local".into(), None, dir.clone());
        let id = app.session.alloc_id();
        app.session.items.push(TranscriptItem::Notice {
            id,
            text: "held".into(),
            is_error: false,
        });
        let path = dir.join("sessions").join(format!("{}.json", app.session.id));
        app.persist_hold.set(true);
        app.persist_session();
        assert!(!path.exists());
        assert!(app.persist_due.get());
        app.persist_hold.set(false);
        if app.persist_due.take() {
            app.persist_session();
        }
        assert!(path.is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
