//! Top-level CLI subcommand handlers extracted from `main.rs`.

use crate::{mode_option, Ctx};
use anyhow::Result;
use envoy_agent::{auth, daemon};
use envoy_core::AgentConfigStore;
use envoy_proto::{EntryKind, JobState, AGENT_VERSION};
use envoy_service::ConduitClient;
use std::io::{Read, Write};
use std::path::Path;

const COPY_PAGE: u64 = envoy_proto::READ_LIMIT;

pub(crate) fn print_store(store: &AgentConfigStore) {
    println!("data_dir: {}", store.root.display());
    println!(
        "enabled:  {}",
        if store.config.enabled {
            "yes"
        } else {
            "no — remote calls are refused"
        }
    );
    println!("max_jobs: {}", store.config.max_jobs);
    if store.config.allow.is_empty() {
        println!("allow:    (none)");
    } else {
        println!("allow:");
        for a in &store.config.allow {
            println!("          {a}");
        }
    }
}

pub(crate) async fn cmd_status(ctx: &Ctx) -> Result<()> {
    let store = AgentConfigStore::open(ctx.data_dir.clone())?;
    print_store(&store);
    println!();
    if daemon::is_running(&ctx.data_dir).await {
        println!(
            "daemon:   running ({})",
            daemon::socket_path(&ctx.data_dir).display()
        );
        let remote = ctx.remote_for("local");
        match remote.jobs("local").await {
            Ok(jobs) => {
                let running = jobs
                    .iter()
                    .filter(|j| matches!(j.state, JobState::Running { .. }))
                    .count();
                let queued = jobs
                    .iter()
                    .filter(|j| matches!(j.state, JobState::Queued { .. }))
                    .count();
                println!(
                    "jobs:     {} known, {running} running, {queued} queued",
                    jobs.len()
                );
            }
            Err(e) => println!("jobs:     unavailable ({e:#})"),
        }
    } else {
        println!(
            "daemon:   stopped ({})",
            daemon::socket_path(&ctx.data_dir).display()
        );
    }

    let conduit = ConduitClient::new(ctx.conduit_socket());
    match conduit.status().await {
        Ok(status) => {
            println!(
                "conduit:  connected ({}), this device {} ({})",
                conduit.socket().display(),
                status.device_name,
                status.device_id
            );
            println!(
                "network:  {} · tun {} · {} peer(s) · vip {}",
                status.connection.as_deref().unwrap_or("unknown"),
                if status.tun.enabled {
                    status.tun.interface.as_deref().unwrap_or("up")
                } else {
                    "off"
                },
                status.peers.len(),
                status.virtual_ip.as_deref().unwrap_or("-")
            );
            match status
                .services
                .iter()
                .find(|s| s.service_id == envoy_proto::AGENT_SERVICE_ID)
            {
                Some(service) => println!(
                    "service:  {} online={} listeners={}",
                    service.name, service.online, service.listeners
                ),
                None => println!("service:  envoy not registered (start `envoy serve`)"),
            }
        }
        Err(_) => println!("conduit:  not running ({})", conduit.socket().display()),
    }
    println!("protocol: v{AGENT_VERSION}");
    println!("sandbox:  {}", envoy_core::sandbox::backend_label());
    Ok(())
}

pub(crate) fn cmd_set_enabled(data_dir: &Path, enabled: bool) -> Result<()> {
    let mut store = AgentConfigStore::open(data_dir.to_path_buf())?;
    store.set_enabled(enabled)?;
    print_store(&store);
    Ok(())
}

pub(crate) fn cmd_allow(data_dir: &Path, caller_id: String) -> Result<()> {
    let mut store = AgentConfigStore::open(data_dir.to_path_buf())?;
    store.allow(caller_id)?;
    print_store(&store);
    Ok(())
}

pub(crate) fn cmd_deny(data_dir: &Path, caller_id: &str) -> Result<()> {
    let mut store = AgentConfigStore::open(data_dir.to_path_buf())?;
    store.deny(caller_id)?;
    print_store(&store);
    Ok(())
}

pub(crate) async fn cmd_devices(ctx: &Ctx) -> Result<()> {
    let client = ConduitClient::new(ctx.conduit_socket());
    let reply = client.devices().await?;
    if reply.devices.is_empty() {
        println!("no devices in this account");
        return Ok(());
    }
    let live: Vec<(String, bool)> = client
        .status()
        .await
        .map(|status| {
            status
                .peers
                .into_iter()
                .map(|peer| {
                    (
                        peer.device_id.clone(),
                        peer.has_service(envoy_proto::AGENT_SERVICE_ID),
                    )
                })
                .collect()
        })
        .unwrap_or_default();

    println!(
        "{:<20} {:<16} {:<16} {:<14} {:<8} {:<8} ME",
        "ID", "NAME", "DNS", "VIP", "STATUS", "AGENT"
    );
    for device in reply.devices {
        let agent = match live.iter().find(|(id, _)| id == &device.device_id) {
            Some((_, true)) => "yes",
            Some((_, false)) => "no",
            None => "offline",
        };
        println!(
            "{:<20} {:<16} {:<16} {:<14} {:<8} {:<8} {}",
            device.device_id,
            device.name,
            device.dns_name.unwrap_or_else(|| "-".into()),
            device.virtual_ip.unwrap_or_else(|| "-".into()),
            device.status,
            agent,
            if device.me { "yes" } else { "" }
        );
    }
    Ok(())
}

pub(crate) async fn cmd_read(ctx: &Ctx, path: &str, offset: u64, limit: u64) -> Result<()> {
    let remote = ctx.remote().await?;
    let (size, mode, data) = remote.read(&ctx.on, path, offset, limit).await?;
    let mut out = std::io::stdout();
    out.write_all(&data)?;
    out.flush()?;
    eprintln!(
        "{} bytes of {} (mode {:o}) from {}",
        data.len(),
        size,
        mode & 0o777,
        ctx.on
    );
    Ok(())
}

pub(crate) async fn cmd_write(
    ctx: &Ctx,
    path: &str,
    content: Option<String>,
    mode: &Option<String>,
) -> Result<()> {
    let mode = mode_option(mode)?;
    let data = match content.as_deref() {
        Some("-") | None => {
            let mut buf = Vec::new();
            std::io::stdin().read_to_end(&mut buf)?;
            buf
        }
        Some(text) => text.as_bytes().to_vec(),
    };
    let remote = ctx.remote().await?;
    let bytes = remote.write(&ctx.on, path, &data, mode).await?;
    println!("wrote {bytes} bytes to {path} on {}", ctx.on);
    Ok(())
}

pub(crate) async fn cmd_ls(ctx: &Ctx, path: &str) -> Result<()> {
    let remote = ctx.remote().await?;
    let entries = remote.list(&ctx.on, path).await?;
    println!("{:<8} {:>10} {:<5} NAME", "KIND", "SIZE", "MODE");
    for e in entries {
        let kind = match e.kind {
            EntryKind::File => "file",
            EntryKind::Dir => "dir",
            EntryKind::Symlink => "symlink",
            EntryKind::Other => "other",
        };
        println!(
            "{:<8} {:>10} {:<5o} {}",
            kind,
            e.size,
            e.mode & 0o777,
            e.name
        );
    }
    Ok(())
}

pub(crate) async fn cmd_copy(
    ctx: &Ctx,
    from: &str,
    from_path: &str,
    to: &str,
    to_path: &str,
    mode: &Option<String>,
) -> Result<()> {
    let mode = mode_option(mode)?;
    let src = ctx.remote_to(from).await?;
    let dst = ctx.remote_to(to).await?;

    let (_, src_mode, first) = src.read(from, from_path, 0, COPY_PAGE).await?;
    let mut writer = dst.open_write(to, to_path, mode.or(Some(src_mode))).await?;
    let mut total = 0u64;
    let mut page = first;
    let mut offset = 0u64;
    loop {
        total += page.len() as u64;
        writer.send(&page).await?;
        offset += page.len() as u64;
        if (page.len() as u64) < COPY_PAGE {
            break;
        }
        let (_, _, next) = src.read(from, from_path, offset, COPY_PAGE).await?;
        if next.is_empty() {
            break;
        }
        page = next;
    }
    writer.finish().await?;
    println!("copied {total} bytes: {from}:{from_path} -> {to}:{to_path}");
    Ok(())
}

pub(crate) async fn cmd_models(data_dir: &Path) -> Result<()> {
    let models = auth::available_models(data_dir);
    let active = auth::resolve_active_provider(data_dir).map(|(_, m)| m);
    if models.is_empty() {
        println!("No models available. Run `envoy login` or type `/login` in the console.");
        return Ok(());
    }
    println!("{:<18} {:<30} {:<10} ACTIVE", "PROVIDER", "MODEL", "AUTH");
    for m in models {
        let is_active = active
            .as_ref()
            .is_some_and(|a| a.provider_id == m.provider_id && a.model_id == m.model_id);
        println!(
            "{:<18} {:<30} {:<10} {}",
            m.provider_id,
            m.model_id,
            m.auth_kind,
            if is_active { "*" } else { "" }
        );
    }
    Ok(())
}
