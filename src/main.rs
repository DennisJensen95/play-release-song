// Sonos Finder and Player
// This script discovers Sonos devices on your network and plays a specific song

// STD
use futures::stream::{self, StreamExt};
use std::net::IpAddr;
use std::net::UdpSocket;
use std::sync::{Arc, Mutex};
use std::thread;

// TPM
use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use clap::{Parser, Subcommand};
use octocrab::Octocrab;
use regex::Regex;
use reqwest::{header, Client};
use tokio::time;
use xml::writer::{EventWriter, XmlEvent};

// SSDP discovery message for UPnP devices
const SSDP_ADDR: &str = "239.255.255.250:1900";
const SSDP_M_SEARCH: &str = "M-SEARCH * HTTP/1.1\r\n\
                             HOST: 239.255.255.250:1900\r\n\
                             MAN: \"ssdp:discover\"\r\n\
                             MX: 3\r\n\
                             ST: urn:schemas-upnp-org:device:ZonePlayer:1\r\n\r\n";

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "Control Sonos speakers and monitor GitHub CI"
)]
struct SonosController {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Stop music on all Sonos speakers
    Kill,

    /// Start playing music on all Sonos speakers
    Start {
        /// Optional track URI to play (defaults to Agnes - Release Me)
        #[arg(short, long)]
        track: Option<String>,
    },

    /// Listen to GitHub CI pipeline and start music on trigger
    Listen {
        /// GitHub repository to monitor (format: owner/repo)
        #[arg(short, long)]
        repo: String,

        /// Branch to monitor
        #[arg(short, long, default_value = "main")]
        branch: String,

        /// Polling interval in seconds
        #[arg(short, long, default_value = "60")]
        interval: u64,
    },
}

// Structure to track the last played time
struct LastPlayedTracker {
    last_played: Option<std::time::Instant>,
    cooldown: std::time::Duration,
}

impl LastPlayedTracker {
    fn new(cooldown_minutes: u64) -> Self {
        Self {
            last_played: None,
            cooldown: std::time::Duration::from_secs(cooldown_minutes * 60),
        }
    }

    fn can_play_now(&self) -> bool {
        match self.last_played {
            None => true,
            Some(instant) => instant.elapsed() >= self.cooldown,
        }
    }

    fn mark_played(&mut self) {
        self.last_played = Some(std::time::Instant::now());
    }

    fn time_until_next_play(&self) -> Option<std::time::Duration> {
        self.last_played.map(|instant| {
            let elapsed = instant.elapsed();
            if elapsed >= self.cooldown {
                std::time::Duration::from_secs(0)
            } else {
                self.cooldown - elapsed
            }
        })
    }
}

// Modified start_music function with cooldown tracking and speaker grouping
async fn start_music(track_uri: &str, tracker: Arc<Mutex<LastPlayedTracker>>) -> Result<()> {
    // Check if we can play now based on the cooldown
    let can_play;
    let wait_time;

    {
        let tracker_guard = tracker.lock().unwrap();
        can_play = tracker_guard.can_play_now();
        wait_time = tracker_guard.time_until_next_play();
    }

    if !can_play {
        let minutes = wait_time.unwrap().as_secs() / 60;
        let seconds = wait_time.unwrap().as_secs() % 60;
        println!(
            "Music was recently played. Please wait {}m {}s before playing again.",
            minutes, seconds
        );
        return Ok(());
    }

    println!("Finding Sonos speakers on your network...");
    let sonos_ips = discover_sonos_devices().await?;

    if sonos_ips.is_empty() {
        println!("No Sonos devices found.");
        return Ok(());
    }

    println!("Found {} Sonos devices:", sonos_ips.len());
    for (i, ip) in sonos_ips.iter().enumerate() {
        let device_info = get_device_info(ip)
            .await
            .unwrap_or_else(|_| format!("Unknown Device at {}", ip));
        println!("[{}] {}", i + 1, device_info);
    }

    // Select the first speaker as the coordinator
    let coordinator_ip = sonos_ips[0].clone();
    let member_ips: Vec<String> = sonos_ips.iter().skip(1).cloned().collect();

    // Group speakers if there are multiple
    if !member_ips.is_empty() {
        println!(
            "Grouping {} speakers with coordinator {}...",
            member_ips.len() + 1,
            coordinator_ip
        );
        match group_speakers(&coordinator_ip, &member_ips).await {
            Ok(_) => println!("Speakers grouped successfully."),
            Err(e) => {
                println!(
                    "Failed to group speakers: {}. Proceeding without grouping.",
                    e
                );
                // Optionally, decide if you want to stop here or try playing on all individually
                // For now, we'll proceed to play on the coordinator only
            }
        }
        // Add a small delay to allow grouping to settle
        time::sleep(std::time::Duration::from_secs(1)).await;
    }

    println!("Playing music on coordinator speaker {}...", coordinator_ip);

    // Send play command only to the coordinator
    match play_track(&coordinator_ip, track_uri).await {
        Ok(_) => println!(
            "Successfully sent play command to coordinator {}",
            coordinator_ip
        ),
        Err(e) => println!(
            "Failed to send play command to coordinator {}: {}",
            coordinator_ip, e
        ),
    }

    println!("Play command sent.");

    // Mark as played after successful playback attempt
    {
        let mut tracker_guard = tracker.lock().unwrap();
        tracker_guard.mark_played();
    }

    Ok(())
}

// Function to check GitHub CI status
async fn check_github_ci(repo: &str, branch: &str) -> Result<bool> {
    // Parse owner and repo name
    let parts: Vec<&str> = repo.split('/').collect();
    if parts.len() != 2 {
        return Err(anyhow::anyhow!(
            "Invalid repository format. Use 'owner/repo'"
        ));
    }

    let owner = parts[0];
    let repo_name = parts[1];

    println!(
        "Checking CI status for {}/{} on branch {}",
        owner, repo_name, branch
    );

    // Initialize GitHub client
    let octocrab = match std::env::var("GITHUB_ACTIONS_TOKEN") {
        Ok(token) => Octocrab::builder().personal_token(token).build()?,
        Err(_) => {
            println!("Warning: No GITHUB_ACTIONS_TOKEN found in environment. Using unauthenticated client with rate limits.");
            Octocrab::builder().build()?
        }
    };

    // Get workflow runs for the repo and branch
    let runs = octocrab
        .workflows(owner, repo_name)
        .list_runs("digitalt-haandvaerk-platform.yaml")
        .branch(branch)
        .send()
        .await?;

    // Check if there are any runs
    if let Some(run) = runs.items.into_iter().next() {
        // Parse run creation time
        let created_at: DateTime<Utc> = run.created_at;
        let now = Utc::now();

        // Check if the run was created in the last 5 minutes and is in progress
        let is_recent = now.signed_duration_since(created_at) < Duration::minutes(20);
        let is_in_progress = run.status == "in_progress" || run.status == "queued";

        println!(
            "Latest run ID: {}, Status: {}, Created: {}",
            run.id, run.status, run.created_at
        );

        if is_recent && is_in_progress {
            println!("Found a recently triggered CI pipeline!");
            return Ok(true);
        }
    }

    Ok(false)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = SonosController::parse();

    // Create a shared tracker with 30-minute cooldown
    let tracker = Arc::new(Mutex::new(LastPlayedTracker::new(30)));

    match &cli.command {
        Commands::Kill => kill_music().await?,
        Commands::Start { track } => {
            let track_uri = track.clone().unwrap_or_else(|| {
                "x-sonos-spotify:spotify:track:4LiJE6pqgsTX3FtukW6bNh?sid=9&flags=8224&sn=7"
                    .to_string()
            });
            start_music(&track_uri, tracker.clone()).await?
        }
        Commands::Listen {
            repo,
            branch,
            interval,
        } => listen_to_ci(repo, branch, *interval, tracker.clone()).await?,
    }

    Ok(())
}

// Kill music implementation
async fn kill_music() -> Result<()> {
    println!("Finding Sonos speakers to stop music...");
    let sonos_ips = discover_sonos_devices().await?;

    if sonos_ips.is_empty() {
        println!("No Sonos devices found.");
        return Ok(());
    }

    println!("Stopping music on {} Sonos devices...", sonos_ips.len());

    let mut stop_tasks = Vec::new();
    for ip in &sonos_ips {
        let ip_clone = ip.clone();
        stop_tasks.push(tokio::spawn(async move {
            match stop_playback(&ip_clone).await {
                Ok(_) => println!("Successfully stopped playback on {}", ip_clone),
                Err(e) => println!("Failed to stop playback on {}: {}", ip_clone, e),
            }
        }));
    }

    // Wait for all stop commands to complete
    for task in stop_tasks {
        let _ = task.await;
    }

    println!("All speakers stopped!");
    Ok(())
}

// Listen to CI implementation
async fn listen_to_ci(
    repo: &str,
    branch: &str,
    interval: u64,
    tracker: Arc<Mutex<LastPlayedTracker>>,
) -> Result<()> {
    println!("Monitoring GitHub CI pipeline for {repo} on branch {branch}");
    println!("Checking every {interval} seconds");

    loop {
        match check_github_ci(repo, branch).await {
            Ok(true) => {
                println!("CI trigger detected! Starting music...");
                start_music(
                    "x-sonos-spotify:spotify:track:4LiJE6pqgsTX3FtukW6bNh?sid=9&flags=8224&sn=7",
                    tracker.clone(),
                )
                .await?;
            }
            Ok(false) => {
                println!("No CI trigger detected. Waiting {} seconds...", interval);
            }
            Err(e) => {
                println!("Error checking CI status: {}", e);
            }
        }
        time::sleep(std::time::Duration::from_secs(interval)).await;
    }
}

async fn discover_sonos_devices() -> Result<Vec<String>> {
    let mut sonos_ips = Vec::new();

    // Create a UDP socket for SSDP discovery
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.set_read_timeout(Some(std::time::Duration::from_secs(3)))?;

    // Send the SSDP discovery message
    socket.send_to(SSDP_M_SEARCH.as_bytes(), SSDP_ADDR)?;

    // Listen for responses
    let mut buf = [0u8; 2048];
    let re = Regex::new(r"LOCATION: http://([0-9]+\.[0-9]+\.[0-9]+\.[0-9]+)").unwrap();

    // Create a timeout for discovery
    let start_time = std::time::Instant::now();
    while start_time.elapsed() < std::time::Duration::from_secs(5) {
        match socket.recv_from(&mut buf) {
            Ok((len, _)) => {
                let response = String::from_utf8_lossy(&buf[..len]);
                if response.contains("Sonos") {
                    if let Some(cap) = re.captures(&response) {
                        if let Some(ip_match) = cap.get(1) {
                            let ip = ip_match.as_str().to_string();
                            if !sonos_ips.contains(&ip) {
                                sonos_ips.push(ip);
                            }
                        }
                    }
                }
            }
            Err(_) => {
                // Timeout or error, continue
                thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }

    // If SSDP didn't find anything, try scanning common local subnets (simple approach)
    if sonos_ips.is_empty() {
        println!("SSDP discovery failed. Trying network scan (this may take a minute)...");

        // Get local IP to determine subnet
        let local_subnet = get_local_subnet()?;
        println!("Scanning subnet: {}/24", local_subnet);

        // Create a client with a short timeout for scanning
        let client = Client::builder()
            .timeout(std::time::Duration::from_millis(500))
            .build()?;

        // Scan the subnet for devices with the Sonos SOAP service
        let mut tasks = Vec::new();
        for i in 1..=254 {
            let ip = format!("{}.{}", local_subnet, i);
            let client = client.clone();
            tasks.push(async move {
                let url = format!("http://{}:1400/xml/device_description.xml", ip);
                match client.get(&url).send().await {
                    Ok(resp) => {
                        if resp.status().is_success() {
                            let text = resp.text().await.unwrap_or_default();
                            if text.contains("Sonos") {
                                return Some(ip);
                            }
                        }
                        None
                    }
                    Err(_) => None,
                }
            });
        }

        // Execute all tasks with a limit on concurrent requests
        let results = stream::iter(tasks)
            .buffer_unordered(50)
            .collect::<Vec<_>>()
            .await;

        for ip in results.into_iter().flatten() {
            sonos_ips.push(ip);
        }
    }

    Ok(sonos_ips)
}

fn get_local_subnet() -> Result<String> {
    // A very simple approach to get the local subnet
    // In a real application, you'd want to enumerate network interfaces

    // Try to connect to a public IP to determine local address
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    let _ = socket.connect("8.8.8.8:80"); // Google's DNS
    let local_addr = socket.local_addr()?;

    if let IpAddr::V4(ipv4) = local_addr.ip() {
        let octets = ipv4.octets();
        Ok(format!("{}.{}.{}", octets[0], octets[1], octets[2]))
    } else {
        // Fallback to a common subnet
        Ok("192.168.1".to_string())
    }
}

async fn get_device_info(ip: &str) -> Result<String> {
    let client = Client::new();
    let url = format!("http://{}:1400/xml/device_description.xml", ip);

    let resp = client
        .get(&url)
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await?;
    let text = resp.text().await?;

    // Extract device name and model using regex (simple approach)
    let name_re = Regex::new(r"<roomName>([^<]+)</roomName>").unwrap();
    let model_re = Regex::new(r"<displayName>([^<]+)</displayName>").unwrap();

    let name = name_re
        .captures(&text)
        .and_then(|cap| cap.get(1))
        .map_or("Unknown", |m| m.as_str());

    let model = model_re
        .captures(&text)
        .and_then(|cap| cap.get(1))
        .map_or("Unknown", |m| m.as_str());

    Ok(format!("{} ({}) - {}", name, model, ip))
}

async fn play_track(ip: &str, track_uri: &str) -> Result<()> {
    // First, we need to set the URI of the track on the coordinator
    println!("Setting track URI {} on {}", track_uri, ip);
    set_av_transport_uri(ip, track_uri).await?;

    // Add a small delay between setting URI and playing
    time::sleep(std::time::Duration::from_millis(500)).await;

    // Then we can play it
    println!("Sending Play command to {}", ip);
    play(ip).await?;

    Ok(())
}

async fn set_av_transport_uri(ip: &str, track_uri: &str) -> Result<()> {
    // The SOAP action for setting the transport URI
    let action = "SetAVTransportURI";
    let service = "AVTransport";

    // Create the SOAP envelope
    let mut writer = Vec::new();
    {
        let mut xml_writer = EventWriter::new(&mut writer);

        // Start SOAP envelope
        xml_writer
            .write(
                XmlEvent::start_element("s:Envelope")
                    .attr("xmlns:s", "http://schemas.xmlsoap.org/soap/envelope/")
                    .attr(
                        "s:encodingStyle",
                        "http://schemas.xmlsoap.org/soap/encoding/",
                    ),
            )
            .unwrap();

        // SOAP body
        xml_writer.write(XmlEvent::start_element("s:Body")).unwrap();

        // Action
        let action_name = format!("u:{}", action);
        let namespace = format!("urn:schemas-upnp-org:service:{}:1", service);
        xml_writer
            .write(
                XmlEvent::start_element(action_name.as_str()).attr("xmlns:u", namespace.as_str()),
            )
            .unwrap();

        // Parameters
        xml_writer
            .write(XmlEvent::start_element("InstanceID"))
            .unwrap();
        xml_writer.write(XmlEvent::Characters("0")).unwrap();
        xml_writer.write(XmlEvent::end_element()).unwrap();

        xml_writer
            .write(XmlEvent::start_element("CurrentURI"))
            .unwrap();
        xml_writer.write(XmlEvent::Characters(track_uri)).unwrap();
        xml_writer.write(XmlEvent::end_element()).unwrap();

        xml_writer
            .write(XmlEvent::start_element("CurrentURIMetaData"))
            .unwrap();
        xml_writer.write(XmlEvent::Characters("")).unwrap();
        xml_writer.write(XmlEvent::end_element()).unwrap();

        // Close tags
        xml_writer.write(XmlEvent::end_element()).unwrap(); // u:action
        xml_writer.write(XmlEvent::end_element()).unwrap(); // s:Body
        xml_writer.write(XmlEvent::end_element()).unwrap(); // s:Envelope
    }

    let body = String::from_utf8(writer)?;

    // Create and send the HTTP request
    let client = Client::new();
    let url = format!("http://{}:1400/MediaRenderer/AVTransport/Control", ip);

    let mut headers = header::HeaderMap::new();
    headers.insert(
        "Content-Type",
        header::HeaderValue::from_static("text/xml; charset=\"utf-8\""),
    );
    headers.insert(
        "SOAPAction",
        header::HeaderValue::from_str(&format!(
            "\"urn:schemas-upnp-org:service:{}:1#{}\"",
            service, action
        ))?,
    );

    let resp = client.post(&url).headers(headers).body(body).send().await?;

    if !resp.status().is_success() {
        println!("Error setting track URI. Status: {}", resp.status());
        println!("Response: {}", resp.text().await?);
        return Err(anyhow::anyhow!("Failed to set track URI"));
    }

    Ok(())
}

async fn play(ip: &str) -> Result<()> {
    // The SOAP action for playing
    let action = "Play";
    let service = "AVTransport";

    // Create the SOAP envelope
    let mut writer = Vec::new();
    {
        let mut xml_writer = EventWriter::new(&mut writer);

        // Start SOAP envelope
        xml_writer
            .write(
                XmlEvent::start_element("s:Envelope")
                    .attr("xmlns:s", "http://schemas.xmlsoap.org/soap/envelope/")
                    .attr(
                        "s:encodingStyle",
                        "http://schemas.xmlsoap.org/soap/encoding/",
                    ),
            )
            .unwrap();

        // SOAP body
        xml_writer.write(XmlEvent::start_element("s:Body")).unwrap();

        // Action
        let action_name = format!("u:{}", action);
        let namespace = format!("urn:schemas-upnp-org:service:{}:1", service);
        xml_writer
            .write(
                XmlEvent::start_element(action_name.as_str()).attr("xmlns:u", namespace.as_str()),
            )
            .unwrap();

        // Parameters
        xml_writer
            .write(XmlEvent::start_element("InstanceID"))
            .unwrap();
        xml_writer.write(XmlEvent::Characters("0")).unwrap();
        xml_writer.write(XmlEvent::end_element()).unwrap();

        xml_writer.write(XmlEvent::start_element("Speed")).unwrap();
        xml_writer.write(XmlEvent::Characters("1")).unwrap();
        xml_writer.write(XmlEvent::end_element()).unwrap();

        // Close tags
        xml_writer.write(XmlEvent::end_element()).unwrap(); // u:action
        xml_writer.write(XmlEvent::end_element()).unwrap(); // s:Body
        xml_writer.write(XmlEvent::end_element()).unwrap(); // s:Envelope
    }

    let body = String::from_utf8(writer)?;

    // Create and send the HTTP request
    let client = Client::new();
    let url = format!("http://{}:1400/MediaRenderer/AVTransport/Control", ip);

    let mut headers = header::HeaderMap::new();
    headers.insert(
        "Content-Type",
        header::HeaderValue::from_static("text/xml; charset=\"utf-8\""),
    );
    headers.insert(
        "SOAPAction",
        header::HeaderValue::from_str(&format!(
            "\"urn:schemas-upnp-org:service:{}:1#{}\"",
            service, action
        ))?,
    );

    let resp = client.post(&url).headers(headers).body(body).send().await?;

    if !resp.status().is_success() {
        println!("Error playing track. Status: {}", resp.status());
        println!("Response: {}", resp.text().await?);
        return Err(anyhow::anyhow!("Failed to play"));
    }

    Ok(())
}

// Add this function to stop playback on a device
async fn stop_playback(ip: &str) -> Result<()> {
    // The SOAP action for stopping
    let action = "Stop";
    let service = "AVTransport";

    // Create the SOAP envelope
    let mut writer = Vec::new();
    {
        let mut xml_writer = EventWriter::new(&mut writer);

        // Start SOAP envelope
        xml_writer
            .write(
                XmlEvent::start_element("s:Envelope")
                    .attr("xmlns:s", "http://schemas.xmlsoap.org/soap/envelope/")
                    .attr(
                        "s:encodingStyle",
                        "http://schemas.xmlsoap.org/soap/encoding/",
                    ),
            )
            .unwrap();

        // SOAP body
        xml_writer.write(XmlEvent::start_element("s:Body")).unwrap();

        // Action
        let action_name = format!("u:{}", action);
        let namespace = format!("urn:schemas-upnp-org:service:{}:1", service);
        xml_writer
            .write(
                XmlEvent::start_element(action_name.as_str()).attr("xmlns:u", namespace.as_str()),
            )
            .unwrap();

        // Parameters
        xml_writer
            .write(XmlEvent::start_element("InstanceID"))
            .unwrap();
        xml_writer.write(XmlEvent::Characters("0")).unwrap();
        xml_writer.write(XmlEvent::end_element()).unwrap();

        // Close tags
        xml_writer.write(XmlEvent::end_element()).unwrap(); // u:action
        xml_writer.write(XmlEvent::end_element()).unwrap(); // s:Body
        xml_writer.write(XmlEvent::end_element()).unwrap(); // s:Envelope
    }

    let body = String::from_utf8(writer)?;

    // Create and send the HTTP request
    let client = Client::new();
    let url = format!("http://{}:1400/MediaRenderer/AVTransport/Control", ip);

    let mut headers = header::HeaderMap::new();
    headers.insert(
        "Content-Type",
        header::HeaderValue::from_static("text/xml; charset=\"utf-8\""),
    );
    headers.insert(
        "SOAPAction",
        header::HeaderValue::from_str(&format!(
            "\"urn:schemas-upnp-org:service:{}:1#{}\"",
            service, action
        ))?,
    );

    let resp = client.post(&url).headers(headers).body(body).send().await?;

    if !resp.status().is_success() {
        println!("Error stopping playback. Status: {}", resp.status());
        println!("Response: {}", resp.text().await?);
        return Err(anyhow::anyhow!("Failed to stop playback"));
    }

    Ok(())
}

async fn group_speakers(coordinator_ip: &str, member_ips: &[String]) -> Result<()> {
    // For each member speaker, set AV transport to the coordinator's URL
    for member_ip in member_ips {
        let uri = format!("x-rincon:{}", get_speaker_uuid(coordinator_ip).await?);
        set_av_transport_uri(member_ip, &uri).await?;
    }
    Ok(())
}

async fn get_speaker_uuid(ip: &str) -> Result<String> {
    let client = Client::new();
    let url = format!("http://{}:1400/xml/device_description.xml", ip);
    let resp = client.get(&url).send().await?;
    let text = resp.text().await?;

    // Extract UUID using regex
    let uuid_re = Regex::new(r"<UDN>uuid:([^<]+)</UDN>").unwrap();
    if let Some(cap) = uuid_re.captures(&text) {
        if let Some(uuid) = cap.get(1) {
            return Ok(uuid.as_str().to_string());
        }
    }
    Err(anyhow::anyhow!("Failed to get speaker UUID"))
}

#[cfg(test)]
pub mod test {
    #[test]
    fn test_vibe_coded_no_test() {
        assert_eq!(
            "Does software really need to work always? 😂",
            "Does software really need to work always? 😂"
        )
    }
}
