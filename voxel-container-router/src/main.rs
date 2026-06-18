use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use std::fs;
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Parser, Debug)]
#[command(
    name = "voxel-container-router",
    about = "Start a containerized router and attach VM data interfaces with Docker macvlan networks"
)]
struct Args {
    /// Docker container name.
    #[arg(long)]
    container: String,

    /// Docker image name.
    #[arg(long)]
    image: String,

    /// Container hostname. Defaults to --container.
    #[arg(long)]
    hostname: Option<String>,

    /// Host interface name prefix to attach to the router container.
    #[arg(long, default_value = "enp")]
    interface_prefix: String,

    /// Docker macvlan network name prefix.
    #[arg(long, default_value = "rtr_")]
    network_prefix: String,

    /// Run the container privileged.
    #[arg(long)]
    privileged: bool,

    /// Environment variable for docker create, e.g. FOO=bar.
    #[arg(long = "env")]
    env: Vec<String>,

    /// Bind mount or named volume for docker create, e.g. /host:/container:rw.
    #[arg(long = "volume")]
    volumes: Vec<String>,

    /// Named Docker volume to create before docker create.
    #[arg(long = "docker-volume")]
    docker_volumes: Vec<String>,

    /// Port publishing for docker create, e.g. 51051:51051.
    #[arg(long = "publish")]
    publishes: Vec<String>,

    /// Container command arguments appended after the image.
    #[arg(long = "cmd", allow_hyphen_values = true)]
    command: Vec<String>,

    /// Command arguments for docker exec readiness check.
    #[arg(long = "readiness-exec", allow_hyphen_values = true)]
    readiness_exec: Vec<String>,

    /// Text that must appear in readiness command stdout.
    #[arg(long)]
    readiness_contains: Option<String>,

    /// Seconds to wait for Docker readiness.
    #[arg(long, default_value_t = 300)]
    docker_timeout_secs: u64,

    /// Seconds to wait for container readiness after start.
    #[arg(long, default_value_t = 300)]
    readiness_timeout_secs: u64,
}

fn main() {
    if let Err(err) = run(Args::parse()) {
        eprintln!("[voxel-container-router] FATAL: {err:#}");
        std::process::exit(1);
    }
}

fn run(args: Args) -> Result<()> {
    log(format!(
        "waiting for Docker for container {}",
        args.container
    ));
    wait_for_docker(Duration::from_secs(args.docker_timeout_secs))?;

    let mgmt = default_route_interface().context("find management interface")?;
    if let Some(iface) = &mgmt {
        log(format!("management interface: {iface} (skipped)"));
    }

    let data_interfaces = data_interfaces(&args.interface_prefix, mgmt.as_deref())?;
    log(format!(
        "data interfaces: {}",
        if data_interfaces.is_empty() {
            "<none>".to_string()
        } else {
            data_interfaces.join(", ")
        }
    ));

    remove_container_if_present(&args.container)?;
    cleanup_old_networks(&args.network_prefix)?;
    for volume in &args.docker_volumes {
        ensure_docker_volume(volume)?;
    }

    let networks = create_macvlan_networks(&args.network_prefix, &data_interfaces)?;
    create_container(&args)?;
    connect_networks(&args.container, &networks)?;
    start_container(&args.container)?;

    wait_for_readiness(&args)?;
    log(format!("{} is ready", args.container));
    Ok(())
}

fn log(msg: impl AsRef<str>) {
    println!("[voxel-container-router] {}", msg.as_ref());
}

fn command_output(program: &str, args: &[&str]) -> Result<Output> {
    Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("spawn {} {}", program, args.join(" ")))
}

fn run_command(program: &str, args: &[&str]) -> Result<()> {
    let output = command_output(program, args)?;
    if !output.status.success() {
        bail!(
            "{} {} failed with status {}: {}",
            program,
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn wait_for_docker(timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if command_output("docker", &["info"])
            .map(|out| out.status.success())
            .unwrap_or(false)
        {
            return Ok(());
        }
        thread::sleep(Duration::from_secs(1));
    }
    bail!("docker did not become ready within {}s", timeout.as_secs())
}

fn default_route_interface() -> Result<Option<String>> {
    let routes = fs::read_to_string("/proc/net/route").context("read /proc/net/route")?;
    for line in routes.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() > 2 && fields[1] == "00000000" {
            return Ok(Some(fields[0].to_string()));
        }
    }
    Ok(None)
}

fn data_interfaces(prefix: &str, mgmt: Option<&str>) -> Result<Vec<String>> {
    let mut interfaces = Vec::new();
    for entry in fs::read_dir("/sys/class/net").context("read /sys/class/net")? {
        let entry = entry.context("read /sys/class/net entry")?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow!("invalid non-utf8 interface name"))?;
        if name == "lo" || !name.starts_with(prefix) || Some(name.as_str()) == mgmt {
            continue;
        }
        interfaces.push(name);
    }
    interfaces.sort();
    Ok(interfaces)
}

fn remove_container_if_present(container: &str) -> Result<()> {
    let exists = command_output("docker", &["inspect", container])
        .map(|out| out.status.success())
        .unwrap_or(false);
    if !exists {
        return Ok(());
    }

    log(format!("removing existing container {container}"));
    let _ = command_output("docker", &["stop", "--time", "30", container]);
    run_command("docker", &["rm", container])
}

fn cleanup_old_networks(prefix: &str) -> Result<()> {
    let output = command_output("docker", &["network", "ls", "--format", "{{.Name}}"])?;
    if !output.status.success() {
        bail!(
            "docker network ls failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    for network in String::from_utf8_lossy(&output.stdout).lines() {
        if !network.starts_with(prefix) {
            continue;
        }
        log(format!("removing existing network {network}"));
        let _ = command_output("docker", &["network", "rm", network]);
    }
    Ok(())
}

fn ensure_docker_volume(volume: &str) -> Result<()> {
    let exists = command_output("docker", &["volume", "inspect", volume])
        .map(|out| out.status.success())
        .unwrap_or(false);
    if exists {
        return Ok(());
    }
    log(format!("creating docker volume {volume}"));
    run_command("docker", &["volume", "create", volume])
}

fn create_macvlan_networks(prefix: &str, interfaces: &[String]) -> Result<Vec<String>> {
    let mut networks = Vec::with_capacity(interfaces.len());
    for iface in interfaces {
        let network = format!("{prefix}{iface}");
        let parent = format!("parent={iface}");
        log(format!("creating macvlan network {network} for {iface}"));
        run_command(
            "docker",
            &[
                "network", "create", "--driver", "macvlan", "-o", &parent, &network,
            ],
        )?;
        networks.push(network);
    }
    Ok(networks)
}

fn create_container(args: &Args) -> Result<()> {
    log(format!(
        "creating container {} from {}",
        args.container, args.image
    ));
    let hostname = args.hostname.as_deref().unwrap_or(&args.container);
    let mut cmd = vec![
        "create".to_string(),
        "--name".to_string(),
        args.container.clone(),
        "-h".to_string(),
        hostname.to_string(),
        "--restart".to_string(),
        "unless-stopped".to_string(),
    ];

    if args.privileged {
        cmd.push("--privileged".to_string());
    }
    for env in &args.env {
        cmd.push("-e".to_string());
        cmd.push(env.clone());
    }
    for volume in &args.volumes {
        cmd.push("-v".to_string());
        cmd.push(volume.clone());
    }
    for publish in &args.publishes {
        cmd.push("-p".to_string());
        cmd.push(publish.clone());
    }
    cmd.push("-t".to_string());
    cmd.push(args.image.clone());
    cmd.extend(args.command.iter().cloned());

    let output = Command::new("docker")
        .args(&cmd)
        .output()
        .context("spawn docker create")?;
    if !output.status.success() {
        bail!(
            "docker {} failed with status {}: {}",
            cmd.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn connect_networks(container: &str, networks: &[String]) -> Result<()> {
    for network in networks {
        log(format!("connecting {container} to network {network}"));
        run_command("docker", &["network", "connect", network, container])?;
    }
    Ok(())
}

fn start_container(container: &str) -> Result<()> {
    log(format!("starting container {container}"));
    run_command("docker", &["start", container])
}

fn container_running(container: &str) -> bool {
    let output = command_output(
        "docker",
        &["inspect", container, "--format", "{{.State.Running}}"],
    );
    output
        .map(|out| out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "true")
        .unwrap_or(false)
}

fn wait_for_readiness(args: &Args) -> Result<()> {
    if args.readiness_exec.is_empty() {
        return Ok(());
    }

    log(format!("waiting for {} readiness", args.container));
    let deadline = Instant::now() + Duration::from_secs(args.readiness_timeout_secs);
    while Instant::now() < deadline {
        if container_running(&args.container) && readiness_check(args)? {
            return Ok(());
        }
        thread::sleep(Duration::from_secs(2));
    }
    bail!(
        "{} did not become ready within {}s",
        args.container,
        args.readiness_timeout_secs
    )
}

fn readiness_check(args: &Args) -> Result<bool> {
    let mut cmd = Command::new("docker");
    cmd.arg("exec")
        .arg(&args.container)
        .args(&args.readiness_exec);
    let output = cmd.output().context("spawn docker exec readiness check")?;
    if !output.status.success() {
        return Ok(false);
    }

    if let Some(needle) = &args.readiness_contains {
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Ok(stdout.contains(needle));
    }

    Ok(true)
}
