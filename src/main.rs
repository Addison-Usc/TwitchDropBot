use std::{collections::{BTreeMap, HashSet}, error::Error, path::{Path, PathBuf}, sync::{Arc, atomic::{AtomicBool, Ordering}}, time::Duration};

use indicatif::{ProgressBar, ProgressStyle};
use tokio::{fs, sync::{Notify, broadcast::{self, Receiver, error::TryRecvError}}, time::{Instant, sleep}};
use tracing::{info};
use tracing_appender::rolling;
use tracing_subscriber::fmt::writer::BoxMakeWriter;
use twitch_gql_rs::{TwitchClient, client_type::ClientType, structs::DropCampaigns};

use crate::{r#static::{Channel, DROP_CASH}, stream::{filter_streams, update_stream}};
mod r#static;
mod stream;
mod web;

const STREAM_SLEEP: u64 = 20;
const MAX_COUNT: u64 = 3;

#[derive(Clone)]
pub(crate) struct SessionControl {
    stop: Arc<AtomicBool>,
}

impl SessionControl {
    pub(crate) fn new() -> Self {
        Self { stop: Arc::new(AtomicBool::new(false)) }
    }

    pub(crate) fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    pub(crate) fn should_stop(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
}

#[derive(Clone)]
pub(crate) struct GameGroup {
    pub display_name: String,
    pub campaigns: Vec<DropCampaigns>,
}

pub(crate) fn group_campaigns(campaigns: Vec<DropCampaigns>) -> BTreeMap<String, GameGroup> {
    let mut grouped: BTreeMap<String, GameGroup> = BTreeMap::new();
    for obj in campaigns {
        if obj.status == "EXPIRED" {
            continue;
        }
        if let Some(group) = grouped.get_mut(&obj.game.id) {
            group.campaigns.push(obj);
        } else {
            grouped.insert(obj.game.id.clone(), GameGroup {
                display_name: obj.game.displayName.clone(),
                campaigns: vec![obj],
            });
        }
    }
    grouped
}

async fn ensure_client(home_dir: &Path) -> Result<TwitchClient, Box<dyn Error>> {
    let path = home_dir.join("save.json");
    if !path.exists() {
        let client_type = ClientType::android_app();
        let mut client = TwitchClient::new(&client_type).await?;
        let get_auth = client.request_device_auth().await?;
        println!("Open {} and enter code {}", get_auth.verification_uri, get_auth.user_code);
        client.auth(get_auth).await?;
        client.save_file(&path).await?;
    }
    let client = TwitchClient::load_from_file(&path).await?;
    Ok(client)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let data_dir = PathBuf::from(std::env::var("DATA_DIR").unwrap_or_else(|_| "data".to_string()));
    if !data_dir.exists() {
        fs::create_dir_all(&data_dir).await?;
    }
    let file_appender = rolling::never(&data_dir, "app.log");
    tracing_subscriber::fmt().with_writer(BoxMakeWriter::new(file_appender)).with_ansi(false).init();

    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|arg| arg == "--cli") {
        run_cli(&data_dir).await?;
    } else {
        web::serve(data_dir).await?;
    }

    Ok(())
}

async fn run_cli(data_dir: &Path) -> Result<(), Box<dyn Error>> {
    let client = ensure_client(data_dir).await?;
    let campaign = client.get_campaign().await?;
    let grouped = group_campaigns(campaign.dropCampaigns);

    for (game_id, group) in &grouped {
        println!("{} | {}", game_id, group.display_name);
    }

    let input: String = dialoguer::Input::new().with_prompt("Select game id").interact_text()?;
    let duration_minutes: u64 = dialoguer::Input::new().with_prompt("How many minutes should the session run? (0 = no limit)").interact_text()?;

    let control = SessionControl::new();
    run_session(Arc::new(client), grouped, &input, duration_minutes, data_dir, control).await?;
    Ok(())
}

pub(crate) async fn run_session(
    client: Arc<TwitchClient>,
    grouped: BTreeMap<String, GameGroup>,
    selected_game_id: &str,
    duration_minutes: u64,
    data_dir: &Path,
    control: SessionControl,
) -> Result<(), Box<dyn Error>> {
    let mut grouped = grouped;
    let group = match grouped.remove(selected_game_id) {
        Some(group) => group,
        None => return Err(format!("Unknown game id {}", selected_game_id).into()),
    };

    if duration_minutes > 0 {
        let control = control.clone();
        let seconds = duration_minutes * 60;
        tokio::spawn(async move {
            sleep(Duration::from_secs(seconds)).await;
            control.stop();
        });
    }

    let (tx_watch, mut rx_watch) = tokio::sync::watch::channel(String::new());
    let drop_campaigns = Arc::new(group.campaigns);
    let drop_cash_dir = data_dir.join("cash.json");

    let (tx, rx1) = broadcast::channel(100);
    let rx2 = tx.subscribe();

    let notify = Arc::new(Notify::new());

    watch_sync(client.clone(), rx1, notify.clone(), control.clone()).await;
    info!("Watch synchronization task has been successfully initiated");
    drop_sync(client.clone(), tx_watch, drop_cash_dir, rx2, notify.clone(), control.clone()).await;
    info!("Drop progress tracker is active");
    filter_streams(client.clone(), drop_campaigns.clone(), control.clone()).await;
    info!("Stream filtering has begun");
    update_stream(drop_campaigns, tx, notify, control.clone()).await;
    info!("Stream priority updated");

    for campaign in drop_campaigns.iter() {
        if control.should_stop() {
            break;
        }
        let mut campaign_details = client.get_campaign_details(&campaign.id).await?;

        let drop_ids_cache = DROP_CASH.lock().await.clone();
        for drop_id_cache in drop_ids_cache {
            let deleted_time_based = campaign_details.timeBasedDrops.iter().filter(|time_based| time_based.id == drop_id_cache).map(|time_based| time_based.id.clone()).collect::<Vec<String>>();
            for delete in deleted_time_based {
                if let Some(pos) = campaign_details.timeBasedDrops.iter().position(|time_based| time_based.id == delete) {
                    campaign_details.timeBasedDrops.remove(pos);
                }
            }
        }

        loop {
            if control.should_stop() {
                break;
            }
            rx_watch.changed().await.unwrap();
            let drop_id = rx_watch.borrow();
            if drop_id.is_empty() {
                sleep(Duration::from_secs(10)).await;
                continue;
            }
            if campaign_details.timeBasedDrops.is_empty() {
                break;
            }
            if let Some(pos) = campaign_details.timeBasedDrops.iter().position(|time_based| time_based.id == *drop_id) {
                campaign_details.timeBasedDrops.remove(pos);
            }
        }
    }

    Ok(())
}

async fn watch_sync(client: Arc<TwitchClient>, mut rx: Receiver<Channel>, notify: Arc<Notify>, control: SessionControl) {
    tokio::spawn(async move {
        let mut old_stream_name = String::new();
        let mut stream_id = String::new();

        let mut watching = match rx.recv().await {
            Ok(w) => w,
            Err(_) => return,
        };
        loop {
            if control.should_stop() {
                break;
            }
            match rx.try_recv() {
                Ok(channel) => watching = channel,
                Err(TryRecvError::Closed) => tracing::error!("Closed"),
                Err(_) => {}
            };

            if old_stream_name.is_empty() || old_stream_name != watching.channel_login {
                info!("Now actively watching channel {}", watching.channel_login);
                old_stream_name = watching.channel_login.clone();
                stream_id.clear();
            }

            if stream_id.is_empty() {
                let stream = retry!(client.get_stream_info(&watching.channel_login));
                if let Some(id) = stream.stream {
                    stream_id = id.id
                } else {
                    notify.notify_one();
                    sleep(Duration::from_secs(STREAM_SLEEP)).await;
                    continue;
                }
            }

            match client.send_watch(&watching.channel_login, &stream_id, &watching.channel_id).await {
                Ok(_) => {
                    sleep(Duration::from_secs(STREAM_SLEEP)).await
                }
                Err(e) => {
                    tracing::error!("{e}");
                    sleep(Duration::from_secs(STREAM_SLEEP)).await;
                }
            }
        }
    });
}

async fn drop_sync(
    client: Arc<TwitchClient>,
    tx: tokio::sync::watch::Sender<String>,
    cash_path: PathBuf,
    mut rx_watch: broadcast::Receiver<Channel>,
    notify: Arc<Notify>,
    control: SessionControl,
) {
    tokio::spawn(async move {
        let mut end_time = Instant::now() + Duration::from_secs(60 * 60);
        let mut old_drop = String::new();

        let bar = ProgressBar::new(1);
        bar.set_style(ProgressStyle::with_template("[{bar:40.cyan/blue}] {percent:.1}% ({pos}/{len} min) {msg}").unwrap());
        bar.set_message("Initialization...");
        bar.enable_steady_tick(Duration::from_millis(500));

        if !cash_path.exists() {
            retry!(fs::write(&cash_path, "[]"));
        } else {
            let mut cash = DROP_CASH.lock().await;
            let cash_str = retry!(fs::read_to_string(&cash_path));
            let cash_vec: HashSet<String> = serde_json::from_str(&cash_str).unwrap();
            *cash = cash_vec;
            drop(cash);
        }

        let tolerance = Duration::from_secs(5 * 60);
        let mut count = 0;

        let mut watching = match rx_watch.recv().await {
            Ok(w) => w,
            Err(_) => return,
        };
        loop {
            if control.should_stop() {
                break;
            }
            match rx_watch.try_recv() {
                Ok(new_watch) => {
                    count = 0;
                    watching = new_watch
                }
                Err(TryRecvError::Closed) => break,
                Err(_) => {}
            }
            let mut cash = DROP_CASH.lock().await;

            let drop_progress = retry!(client.get_current_drop_progress_on_channel(&watching.channel_login, &watching.channel_id));

            if drop_progress.dropID.is_empty() {
                count += 1;
                if count >= MAX_COUNT {
                    drop(cash);
                    notify.notify_one();
                    count = 0;
                    continue;
                } else {
                    drop(cash);
                    sleep(Duration::from_secs(5)).await;
                    continue;
                }
            }

            if old_drop.is_empty() {
                old_drop = drop_progress.dropID.to_string()
            }

            let mut need_update = false;

            if end_time <= Instant::now() || old_drop != drop_progress.dropID && !cash.contains(&drop_progress.dropID) {
                retry!(claim_drop(&client, &old_drop));
                info!("Drop claimed: {}", old_drop);
                tx.send(old_drop.to_string()).unwrap();
                cash.insert(old_drop.to_string());
                old_drop = drop_progress.dropID.to_string();
                need_update = true;

                let cash_string_writer = serde_json::to_string_pretty(&cash.clone()).unwrap();
                retry!(fs::write(&cash_path, cash_string_writer.clone()));
                drop(cash);
            }

            bar.set_length(drop_progress.requiredMinutesWatched);
            bar.set_position(drop_progress.currentMinutesWatched);
            bar.set_message(format!("DropID: {}", drop_progress.dropID));

            if end_time <= Instant::now() + tolerance || Instant::now() <= end_time + tolerance && need_update {
                let reaming = drop_progress.requiredMinutesWatched.saturating_sub(drop_progress.currentMinutesWatched);
                end_time = Instant::now() + Duration::from_secs(reaming * 60);
            }

            sleep(Duration::from_secs(30)).await;
        }
    });
}

async fn claim_drop(client: &Arc<TwitchClient>, drop_progress_id: &str) -> Result<(), Box<dyn Error + Send + Sync>> {
    loop {
        let inv = retry!(client.get_inventory());
        for in_progress in inv.inventory.dropCampaignsInProgress {
            for time_based in in_progress.timeBasedDrops {
                if time_based.id == drop_progress_id {
                    if let Some(id) = time_based.self_drop.dropInstanceID {
                        loop {
                            match client.claim_drop(&id).await {
                                Ok(_) => return Ok(()),
                                Err(twitch_gql_rs::error::ClaimDropError::DropAlreadyClaimed) => return Ok(()),
                                Err(e) => tracing::error!("{e}"),
                            }
                            sleep(Duration::from_secs(5)).await
                        }
                    }
                }
            }
        }
    }
}
