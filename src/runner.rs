//! Runs a list of links with the engine. The CLI and the GUI both use this.

use crate::api::Api;
use crate::backoff::Backoff;
use crate::episode::{download_episode_with_retry, download_season, episode_label, hint_for, Failure, Options, Summary};
use crate::ui;
use crate::util::{parse_url, preferred_request_locale};
use anyhow::Result;

/// Everything one run needs.
pub struct Plan {
    pub api: Api,
    pub backoff: Backoff,
    pub opts: Options,
    /// 0 means every season of a series.
    pub season: i64,
    /// Locale sent with the season and series requests.
    pub primary_audio: String,
    pub primary_subs: String,
}

impl Plan {
    /// Builds a plan, picking the locales that the season requests will use.
    pub fn new(api: Api, backoff: Backoff, opts: Options, season: i64) -> Self {
        let primary_audio = preferred_request_locale(&opts.audio_langs, "ja-JP");
        let primary_subs = preferred_request_locale(&opts.subs_langs, "en-US");
        Self { api, backoff, opts, season, primary_audio, primary_subs }
    }
}

/// Port of Go's processUrl. Metadata and season fetch failures are fatal for that
/// URL, as in Go. Episode results are recorded in `summary`.
pub async fn process_url(plan: &Plan, url: &str, summary: &mut Summary) -> Result<()> {
    let Some((kind, id)) = parse_url(url) else {
        ui::warn(format!("Invalid URL (must be /watch/ or /series/): {url}"));
        return Ok(());
    };

    if kind == "watch" {
        let info = plan.api.episode_info(id).await?;
        let label = episode_label(&info);
        let outcome = download_episode_with_retry(&plan.api, &plan.backoff, id, &info, &plan.opts).await;
        summary.record(label, outcome);
        return Ok(());
    }

    let seasons = plan.api.seasons(id, &plan.primary_audio, &plan.primary_subs).await?;

    if plan.season != 0 {
        let Some(season) = seasons.iter().find(|s| s.season_number == plan.season) else {
            ui::warn(format!("This series has no season {}.", plan.season));
            return Ok(());
        };
        let episodes = plan.api.season_episodes(&season.id, &plan.primary_audio, &plan.primary_subs).await?;
        download_season(&plan.api, &plan.backoff, &plan.opts, &episodes, summary).await;
    } else {
        ui::info("No season number given, so every season will be downloaded.");
        for season in &seasons {
            let episodes = plan.api.season_episodes(&season.id, &plan.primary_audio, &plan.primary_subs).await?;
            download_season(&plan.api, &plan.backoff, &plan.opts, &episodes, summary).await;
        }
    }
    Ok(())
}

/// Records a link that failed before any episode could start, so the run goes on.
pub fn record_url_failure(summary: &mut Summary, url: &str, e: &anyhow::Error) {
    let message = format!("{e:#}");
    let hint = hint_for(e);
    ui::error_block(format!("Could not download {url}"), &message, hint);
    summary.failed.push(Failure { label: url.to_string(), message, hint });
}

/// Runs every link in order and returns the totals.
pub async fn run_urls(plan: &Plan, urls: &[String]) -> Summary {
    let mut summary = Summary::default();
    let many = urls.len() > 1;
    for (i, url) in urls.iter().enumerate() {
        if many {
            ui::title(format!("[{}/{}] {url}", i + 1, urls.len()));
        }
        if let Err(e) = process_url(plan, url, &mut summary).await {
            record_url_failure(&mut summary, url, &e);
        }
    }
    summary
}
