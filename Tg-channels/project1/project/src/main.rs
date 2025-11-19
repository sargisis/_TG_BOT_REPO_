use std::collections::HashSet;
use std::env;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Client;
use rss::{Channel, Item};
use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::sync::Mutex;
use tokio::time::sleep;

const DEFAULT_FEED: &str = "https://feeds.bbci.co.uk/news/technology/rss.xml";
const DEFAULT_MAX_POSTS: usize = 3;
const DEFAULT_CHECK_INTERVAL_SECS: u64 = 300;
const DEFAULT_STATE_DIR: &str = "state";

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let config = Config::from_env();
    let telegram_client = Client::builder()
        .timeout(Duration::from_secs(25))
        .user_agent("sss_agent_bot/2.0")
        .build()
        .context("failed to build Telegram client")?;
    let news_service = NewsService::new(config.feed_url.clone(), config.max_posts)?;

    let tracker = FeedTracker::load(config.tracker_path()).await?;
    let subscribers = SubscriberStore::load(config.subscriber_path()).await?;

    let mut bot = Bot::new(telegram_client, config, news_service, tracker, subscribers);
    bot.run().await
}

#[derive(Debug, Clone)]
struct Config {
    bot_token: String,
    feed_url: String,
    max_posts: usize,
    check_interval: Duration,
    state_dir: PathBuf,
}

impl Config {
    fn from_env() -> Self {
        let bot_token = env::var("TELEGRAM_BOT_TOKEN").expect("TELEGRAM_BOT_TOKEN is required");
        let feed_url = env::var("FEED_URL").unwrap_or_else(|_| DEFAULT_FEED.to_string());
        let max_posts = env::var("MAX_POSTS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_MAX_POSTS);
        let check_interval = env::var("CHECK_INTERVAL_SECS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_CHECK_INTERVAL_SECS);
        let state_dir = env::var("STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_STATE_DIR));

        Self {
            bot_token,
            feed_url,
            max_posts,
            check_interval: Duration::from_secs(check_interval),
            state_dir,
        }
    }

    fn tracker_path(&self) -> PathBuf {
        self.state_dir.join("last_seen.json")
    }

    fn subscriber_path(&self) -> PathBuf {
        self.state_dir.join("subscribers.json")
    }
}

struct Bot {
    ctx: Arc<BotContext>,
    offset: i64,
}

impl Bot {
    fn new(
        client: Client,
        config: Config,
        news: NewsService,
        tracker: FeedTracker,
        subscribers: SubscriberStore,
    ) -> Self {
        let ctx = Arc::new(BotContext {
            client,
            config,
            news,
            tracker,
            subscribers,
        });

        Self { ctx, offset: 0 }
    }

    async fn run(&mut self) -> Result<()> {
        Self::spawn_auto_updates(self.ctx.clone());

        loop {
            let updates = self.fetch_updates().await?;
            if updates.is_empty() {
                sleep(Duration::from_millis(800)).await;
                continue;
            }

            for update in updates {
                self.offset = update.update_id + 1;
                if let Some(callback) = update.callback_query {
                    self.handle_callback(callback).await?;
                } else if let Some(message) = update.message {
                    self.handle_message(message).await?;
                }
            }
        }
    }

    fn spawn_auto_updates(ctx: Arc<BotContext>) {
        tokio::spawn(async move {
            loop {
                if let Err(err) = ctx.broadcast_latest_if_new().await {
                    eprintln!("auto update failed: {err:#}");
                }
                sleep(ctx.config.check_interval).await;
            }
        });
    }

    fn api_url(&self, method: &str) -> String {
        self.ctx.api_url(method)
    }

    async fn fetch_updates(&self) -> Result<Vec<Update>> {
        let response: UpdatesResponse = self
            .ctx
            .client
            .get(self.api_url("getUpdates"))
            .query(&[("offset", self.offset), ("timeout", 25)])
            .send()
            .await
            .context("telegram getUpdates request failed")?
            .error_for_status()
            .context("telegram rejected getUpdates request")?
            .json()
            .await
            .context("telegram returned malformed getUpdates payload")?;

        Ok(response.result)
    }

    async fn handle_message(&self, message: Message) -> Result<()> {
        let chat_id = message.chat.id;
        self.ctx.subscribers.add(chat_id).await?;

        let text = message.text.unwrap_or_default();
        if text.starts_with("/start") {
            self.send_welcome(chat_id).await?;
        } else if text.starts_with("/latest") {
            self.send_curated_brief(chat_id).await?;
        } else if text.starts_with("/panel") {
            self.send_welcome(chat_id).await?;
        } else if text.starts_with("/help") {
            self.send_mission_brief(chat_id).await?;
        } else {
            self.ctx
                .send_message(
                    chat_id,
                    "🛰️ Command not recognized. Tap a panel below to steer the bot.",
                    Some(self.ctx.hero_keyboard()),
                    true,
                )
                .await?;
        }

        Ok(())
    }

    async fn handle_callback(&self, callback: CallbackQuery) -> Result<()> {
        if let Some(message) = &callback.message {
            self.ctx.subscribers.add(message.chat.id).await?;
        }

        let chat_id = callback
            .message
            .as_ref()
            .map(|message| message.chat.id)
            .context("callback query missing chat context")?;
        let data = callback.data.as_deref().unwrap_or_default();

        match data {
            "auto_pulse" => {
                self.send_auto_pulse(chat_id).await?;
                self.ctx
                    .answer_callback(&callback.id, "AutoPulse armed ⚡")
                    .await?;
            }
            "ai_recap" => {
                self.send_curated_brief(chat_id).await?;
                self.ctx
                    .answer_callback(&callback.id, "Rendering recap ✨")
                    .await?;
            }
            "latest" | "refresh" => {
                self.send_curated_brief(chat_id).await?;
                self.ctx
                    .answer_callback(&callback.id, "Feed refreshed 🔄")
                    .await?;
            }
            "settings" => {
                self.send_mission_brief(chat_id).await?;
                self.ctx
                    .answer_callback(&callback.id, "Mission control opened ⚙️")
                    .await?;
            }
            "tips" => {
                self.ctx
                    .send_message(
                        chat_id,
                        "💡 Tip: Add this bot to any channel as admin and tap AutoPulse for autonomous drops.",
                        Some(self.ctx.secondary_keyboard()),
                        true,
                    )
                    .await?;
                self.ctx.answer_callback(&callback.id, "Noted ✅").await?;
            }
            "support" => {
                self.ctx
                    .send_message(
                        chat_id,
                        "❤️ Thanks for flying with Daily Pulse. Share feedback via @DailyPulseHQ.",
                        Some(self.ctx.secondary_keyboard()),
                        true,
                    )
                    .await?;
                self.ctx
                    .answer_callback(&callback.id, "Appreciated 🙏")
                    .await?;
            }
            _ => {
                self.ctx.answer_callback(&callback.id, "Roger that").await?;
            }
        }

        Ok(())
    }

    async fn send_welcome(&self, chat_id: i64) -> Result<()> {
        let welcome = "🛫 Daily Pulse HUD\n\nAviation-inspired mission console for your AI news bot:\n• AutoPulse schedules clean drops\n• AI Recap condenses every story\n• Live cards keep channels vibrant\n\nChoose a flight plan:";
        self.ctx
            .send_message(chat_id, welcome, Some(self.ctx.hero_keyboard()), true)
            .await
    }

    async fn send_auto_pulse(&self, chat_id: i64) -> Result<()> {
        let text = "🚀 AutoPulse armed. The bot will fetch the freshest tech bursts and prep them for launch. Tap ‘Refresh’ anytime for a manual sweep.";
        self.ctx
            .send_message(chat_id, text, Some(self.ctx.secondary_keyboard()), true)
            .await
    }

    async fn send_curated_brief(&self, chat_id: i64) -> Result<()> {
        let stories = self.ctx.news.fetch_fresh_headlines().await?;

        if stories.is_empty() {
            self.ctx
                .send_message(
                    chat_id,
                    "🌫️ Radar quiet. No fresh stories from the feed yet.",
                    Some(self.ctx.secondary_keyboard()),
                    true,
                )
                .await?;
            return Ok(());
        }

        let payload = BotContext::format_curated(&stories, "🧠 AI Recap");
        self.ctx
            .send_message(
                chat_id,
                &payload,
                Some(self.ctx.secondary_keyboard()),
                false,
            )
            .await?;
        Ok(())
    }

    async fn send_mission_brief(&self, chat_id: i64) -> Result<()> {
        let brief = format!(
            "⚙️ Mission Control\n• Source feed: {}\n• Headlines per flight: {}\n• Commands: /start, /panel, /latest, /help\n\nInvite me to your channel as admin and tap AutoPulse to broadcast.",
            self.ctx.config.feed_url, self.ctx.config.max_posts
        );
        self.ctx
            .send_message(chat_id, &brief, Some(self.ctx.secondary_keyboard()), true)
            .await
    }
}

struct BotContext {
    client: Client,
    config: Config,
    news: NewsService,
    tracker: FeedTracker,
    subscribers: SubscriberStore,
}

impl BotContext {
    fn api_url(&self, method: &str) -> String {
        format!(
            "https://api.telegram.org/bot{}/{}",
            self.config.bot_token, method
        )
    }

    fn hero_keyboard(&self) -> InlineKeyboardMarkup {
        InlineKeyboardMarkup {
            inline_keyboard: vec![
                vec![
                    InlineKeyboardButton::callback("🚀 AutoPulse", "auto_pulse"),
                    InlineKeyboardButton::callback("🧠 AI Recap", "ai_recap"),
                ],
                vec![
                    InlineKeyboardButton::callback("🗞️ News", "latest"),
                    InlineKeyboardButton::callback("⚙️ Mission Control", "settings"),
                ],
                vec![
                    InlineKeyboardButton::callback("💡 Tips", "tips"),
                    InlineKeyboardButton::callback("❤️ Support", "support"),
                ],
                vec![
                    InlineKeyboardButton::url("🌐 Source Feed", self.config.feed_url.clone()),
                    InlineKeyboardButton::callback("📡 Refresh", "refresh"),
                ],
            ],
        }
    }

    fn secondary_keyboard(&self) -> InlineKeyboardMarkup {
        InlineKeyboardMarkup {
            inline_keyboard: vec![
                vec![
                    InlineKeyboardButton::callback("🗞️ News", "latest"),
                    InlineKeyboardButton::callback("📡 Refresh", "refresh"),
                ],
                vec![
                    InlineKeyboardButton::callback("⚙️ Settings", "settings"),
                    InlineKeyboardButton::url("🌐 Feed", self.config.feed_url.clone()),
                ],
            ],
        }
    }

    async fn send_message(
        &self,
        chat_id: i64,
        text: &str,
        reply_markup: Option<InlineKeyboardMarkup>,
        disable_preview: bool,
    ) -> Result<()> {
        let payload = SendMessagePayload {
            chat_id,
            text,
            parse_mode: None,
            reply_markup,
            disable_web_page_preview: disable_preview,
        };

        self.client
            .post(self.api_url("sendMessage"))
            .json(&payload)
            .send()
            .await
            .context("telegram sendMessage failed")?
            .error_for_status()
            .context("telegram rejected sendMessage payload")?;

        Ok(())
    }

    async fn answer_callback(&self, query_id: &str, text: &str) -> Result<()> {
        let payload = AnswerCallbackPayload {
            callback_query_id: query_id,
            text,
            show_alert: false,
        };

        self.client
            .post(self.api_url("answerCallbackQuery"))
            .json(&payload)
            .send()
            .await
            .context("telegram answerCallbackQuery failed")?
            .error_for_status()
            .context("telegram rejected callback response")?;

        Ok(())
    }

    async fn broadcast_latest_if_new(&self) -> Result<()> {
        let stories = self.news.fetch_fresh_headlines().await?;
        if stories.is_empty() {
            return Ok(());
        }

        let newest_id = stories.first().map(|story| story.id.clone());
        let last_known = self.tracker.last_id().await;
        let mut fresh = Vec::new();
        for story in stories.iter() {
            if let Some(last) = &last_known {
                if &story.id == last {
                    break;
                }
            }
            fresh.push(story.clone());
        }

        if fresh.is_empty() {
            if last_known.is_none() {
                self.tracker.update(newest_id).await?;
            }
            return Ok(());
        }

        fresh.reverse();
        let message = BotContext::format_curated(&fresh, "🛰️ AutoPulse Dispatch");
        let subscribers = self.subscribers.all().await;
        if subscribers.is_empty() {
            self.tracker.update(newest_id).await?;
            return Ok(());
        }

        for chat in subscribers {
            self.send_message(chat, &message, Some(self.secondary_keyboard()), false)
                .await?;
        }

        self.tracker.update(newest_id).await?;
        Ok(())
    }

    fn format_curated(items: &[Story], heading: &str) -> String {
        let mut payload = String::new();
        payload.push_str(heading);
        payload.push_str("\n\n");
        for (index, story) in items.iter().enumerate() {
            let marker = match index {
                0 => "①",
                1 => "②",
                2 => "③",
                3 => "④",
                _ => "•",
            };
            payload.push_str(&format!(
                "{} {}\n{}\n🔗 {}\n\n",
                marker, story.title, story.summary, story.link
            ));
        }
        payload
    }
}

#[derive(Clone)]
struct FeedTracker {
    path: PathBuf,
    state: Arc<Mutex<Option<String>>>,
}

impl FeedTracker {
    async fn load(path: PathBuf) -> Result<Self> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).await?;
        }
        let last_id = match fs::read_to_string(&path).await {
            Ok(raw) => {
                serde_json::from_str::<TrackerFile>(&raw)
                    .unwrap_or_else(|_| TrackerFile { last_id: None })
                    .last_id
            }
            Err(_) => None,
        };

        Ok(Self {
            path,
            state: Arc::new(Mutex::new(last_id)),
        })
    }

    async fn last_id(&self) -> Option<String> {
        self.state.lock().await.clone()
    }

    async fn update(&self, new_id: Option<String>) -> Result<()> {
        let mut guard = self.state.lock().await;
        *guard = new_id.clone();
        let payload = serde_json::to_string_pretty(&TrackerFile { last_id: new_id })?;
        fs::write(&self.path, payload).await?;
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct TrackerFile {
    last_id: Option<String>,
}

#[derive(Clone)]
struct SubscriberStore {
    path: PathBuf,
    inner: Arc<Mutex<HashSet<i64>>>,
}

impl SubscriberStore {
    async fn load(path: PathBuf) -> Result<Self> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).await?;
        }
        let set = match fs::read_to_string(&path).await {
            Ok(raw) => {
                let parsed: SubscriberFile =
                    serde_json::from_str(&raw).unwrap_or_else(|_| SubscriberFile {
                        subscribers: vec![],
                    });
                parsed.subscribers.into_iter().collect()
            }
            Err(_) => HashSet::new(),
        };

        Ok(Self {
            path,
            inner: Arc::new(Mutex::new(set)),
        })
    }

    async fn add(&self, chat_id: i64) -> Result<()> {
        let mut guard = self.inner.lock().await;
        if guard.insert(chat_id) {
            self.persist(&guard).await?;
        }
        Ok(())
    }

    async fn all(&self) -> Vec<i64> {
        self.inner.lock().await.iter().copied().collect()
    }

    async fn persist(&self, set: &HashSet<i64>) -> Result<()> {
        let payload = SubscriberFile {
            subscribers: set.iter().copied().collect(),
        };
        let data = serde_json::to_string_pretty(&payload)?;
        fs::write(&self.path, data).await?;
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct SubscriberFile {
    subscribers: Vec<i64>,
}

#[derive(Clone)]
struct NewsService {
    http: Client,
    feed_url: String,
    max_posts: usize,
}

impl NewsService {
    fn new(feed_url: String, max_posts: usize) -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent("DailyPulseFeed/2.0")
            .build()
            .context("failed to build feed client")?;

        Ok(Self {
            http,
            feed_url,
            max_posts,
        })
    }

    async fn fetch_fresh_headlines(&self) -> Result<Vec<Story>> {
        let response = self.http.get(&self.feed_url).send().await;
        let channel = match response {
            Ok(resp) => {
                let xml = resp.error_for_status()?.text().await?;
                Channel::read_from(Cursor::new(xml.as_bytes())).context("could not parse feed")?
            }
            Err(err) => {
                eprintln!("Feed request failed ({err}), using fallback sample");
                Channel::read_from(Cursor::new(Self::fallback_feed().as_bytes()))
                    .context("fallback feed parsing failed")?
            }
        };

        let stories = channel
            .items()
            .iter()
            .take(self.max_posts)
            .map(Story::from_item)
            .collect();

        Ok(stories)
    }

    fn fallback_feed() -> &'static str {
        r#"<?xml version="1.0" encoding="UTF-8" ?>
<rss version="2.0">
  <channel>
    <title>Daily Pulse Sandbox</title>
    <link>https://example.com</link>
    <description>Offline sample news items used when the RSS fetch fails.</description>
    <item>
      <title>AI powered newsletters explode in popularity</title>
      <link>https://example.com/ai-newsletters</link>
      <description>More creators are building themed news channels driven by AI summaries. They want fully automated pipelines, elegant formatting, and multi-vertical coverage.</description>
    </item>
    <item>
      <title>Telegram bots evolve into newsroom assistants</title>
      <link>https://example.com/tg-bots</link>
      <description>Developers now mix RSS feeds, Codex style summarizers, and Telegram APIs to ship curated digests for auto, sports, tech, and local news audiences.</description>
    </item>
  </channel>
</rss>"#
    }
}

#[derive(Debug, Clone)]
struct Story {
    id: String,
    title: String,
    summary: String,
    link: String,
}

impl Story {
    fn from_item(item: &Item) -> Self {
        let id = item
            .guid()
            .map(|guid| guid.value().to_string())
            .or_else(|| item.link().map(|link| link.to_string()))
            .unwrap_or_else(|| item.title().unwrap_or("no-id").to_string());
        let title = item.title().unwrap_or("Untitled Dispatch").to_string();
        let link = item
            .link()
            .unwrap_or("https://example.com/news")
            .to_string();
        let summary = AISummarizer::summarize(item);

        Self {
            id,
            title,
            summary,
            link,
        }
    }
}

struct AISummarizer;

impl AISummarizer {
    fn summarize(item: &Item) -> String {
        let description = item
            .description()
            .or_else(|| item.content())
            .or_else(|| item.title())
            .unwrap_or("Источник не прислал деталей.");

        let sentences: Vec<&str> = description
            .split(|ch| matches!(ch, '.' | '!' | '?'))
            .map(str::trim)
            .filter(|sentence| !sentence.is_empty())
            .collect();

        let summary = sentences
            .iter()
            .take(2)
            .map(|sentence| format!("{}.", sentence.trim_end_matches('.')))
            .collect::<Vec<_>>()
            .join(" ");

        Self::truncate(summary.trim(), 320)
    }

    fn truncate(text: &str, max_chars: usize) -> String {
        if text.is_empty() {
            return "Источник лаконичен — подробности скоро.".to_string();
        }

        let mut result = String::with_capacity(max_chars + 3);
        let mut count = 0;
        for ch in text.chars() {
            if count >= max_chars {
                break;
            }
            result.push(ch);
            count += 1;
        }

        if result.len() < text.len() {
            result.push_str("...");
        }

        result
    }
}

#[derive(Serialize)]
struct SendMessagePayload<'a> {
    chat_id: i64,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    parse_mode: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_markup: Option<InlineKeyboardMarkup>,
    #[serde(default)]
    disable_web_page_preview: bool,
}

#[derive(Serialize)]
struct AnswerCallbackPayload<'a> {
    callback_query_id: &'a str,
    text: &'a str,
    #[serde(default)]
    show_alert: bool,
}

#[derive(Debug, Serialize)]
struct InlineKeyboardMarkup {
    inline_keyboard: Vec<Vec<InlineKeyboardButton>>,
}

#[derive(Debug, Serialize)]
struct InlineKeyboardButton {
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    callback_data: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
}

impl InlineKeyboardButton {
    fn callback(text: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            callback_data: Some(data.into()),
            url: None,
        }
    }

    fn url(text: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            callback_data: None,
            url: Some(url.into()),
        }
    }
}

#[derive(Debug, Deserialize)]
struct UpdatesResponse {
    #[allow(dead_code)]
    ok: bool,
    #[serde(default)]
    result: Vec<Update>,
}

#[derive(Debug, Deserialize)]
struct Update {
    update_id: i64,
    #[serde(default)]
    message: Option<Message>,
    #[serde(default)]
    callback_query: Option<CallbackQuery>,
}

#[derive(Debug, Deserialize)]
struct Message {
    #[allow(dead_code)]
    message_id: i64,
    chat: Chat,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Chat {
    id: i64,
}

#[derive(Debug, Deserialize)]
struct CallbackQuery {
    id: String,
    #[serde(default)]
    data: Option<String>,
    #[serde(default)]
    message: Option<Message>,
}
