//! DASH MPD parsing for both manifest shapes Crunchyroll serves:
//! * segmented: SegmentTemplate + SegmentTimeline on the AdaptationSet
//! * on-demand: one file per Representation addressed by SegmentBase byte ranges

use anyhow::{anyhow, bail, Result};
use serde::Deserialize;

pub const WIDEVINE_SCHEME_SUFFIX: &str = "edef8ba9-79d6-4ace-a3c8-27dcd51d21ed";

#[derive(Debug, Deserialize)]
#[serde(rename = "MPD")]
pub struct Mpd {
    #[serde(rename = "Period", default)]
    pub periods: Vec<Period>,
}

#[derive(Debug, Deserialize)]
pub struct Period {
    #[serde(rename = "AdaptationSet", default)]
    pub adaptation_sets: Vec<AdaptationSet>,
}

#[derive(Debug, Deserialize)]
pub struct AdaptationSet {
    #[serde(rename = "@contentType", default)]
    pub content_type: Option<String>,
    #[serde(rename = "@mimeType", default)]
    pub mime_type: Option<String>,
    #[serde(rename = "SegmentTemplate", default)]
    pub segment_template: Option<SegmentTemplate>,
    #[serde(rename = "ContentProtection", default)]
    pub content_protections: Vec<ContentProtection>,
    #[serde(rename = "Representation", default)]
    pub representations: Vec<Representation>,
}

#[derive(Debug, Deserialize)]
pub struct SegmentTemplate {
    #[serde(rename = "@initialization", default)]
    pub initialization: String,
    #[serde(rename = "@media", default)]
    pub media: String,
    #[serde(rename = "SegmentTimeline", default)]
    pub timeline: Option<SegmentTimeline>,
}

#[derive(Debug, Deserialize)]
pub struct SegmentTimeline {
    #[serde(rename = "S", default)]
    pub s: Vec<TimelineS>,
}

#[derive(Debug, Deserialize)]
pub struct TimelineS {
    #[serde(rename = "@r", default)]
    pub r: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct Representation {
    #[serde(rename = "@id", default)]
    pub id: String,
    #[serde(rename = "@bandwidth", default)]
    pub bandwidth: Option<u64>,
    #[serde(rename = "@height", default)]
    pub height: Option<u64>,
    #[serde(rename = "BaseURL", default)]
    pub base_url: Option<String>,
    #[serde(rename = "SegmentBase", default)]
    pub segment_base: Option<SegmentBase>,
    #[serde(rename = "ContentProtection", default)]
    pub content_protections: Vec<ContentProtection>,
}

#[derive(Debug, Deserialize)]
pub struct SegmentBase {
    #[serde(rename = "@indexRange", default)]
    pub index_range: Option<String>,
    #[serde(rename = "Initialization", default)]
    pub initialization: Option<ByteRangeEl>,
}

#[derive(Debug, Deserialize)]
pub struct ByteRangeEl {
    #[serde(rename = "@range", default)]
    pub range: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ContentProtection {
    #[serde(rename = "@schemeIdUri", default)]
    pub scheme_id_uri: Option<String>,
    #[serde(rename = "pssh", default)]
    pub cenc_pssh: Option<String>,
}

impl Mpd {
    pub fn parse(xml: &str) -> Result<Self> {
        quick_xml::de::from_str(xml).map_err(|e| anyhow!("parse MPD: {e}"))
    }

    pub fn first_period(&self) -> Result<&Period> {
        self.periods
            .first()
            .ok_or_else(|| anyhow!("manifest has no Period"))
    }

    /// On-demand manifests lack a SegmentTemplate on the first AdaptationSet.
    pub fn is_on_demand(&self) -> bool {
        match self.periods.first().and_then(|p| p.adaptation_sets.first()) {
            Some(a) => a.segment_template.is_none(),
            None => false,
        }
    }

    /// Finds the Widevine PSSH, preferring the Widevine scheme over any other.
    pub fn pssh(&self) -> Option<String> {
        let period = self.periods.first()?;
        for set in &period.adaptation_sets {
            if let Some(p) = pssh_from(&set.content_protections) {
                return Some(p);
            }
            for rep in &set.representations {
                if let Some(p) = pssh_from(&rep.content_protections) {
                    return Some(p);
                }
            }
        }
        None
    }
}

fn pssh_from(protections: &[ContentProtection]) -> Option<String> {
    let mut fallback = None;
    for cp in protections {
        let Some(pssh) = &cp.cenc_pssh else { continue };
        if cp
            .scheme_id_uri
            .as_deref()
            .is_some_and(|s| s.contains(WIDEVINE_SCHEME_SUFFIX))
        {
            return Some(pssh.clone());
        }
        if fallback.is_none() {
            fallback = Some(pssh.clone());
        }
    }
    fallback
}

/// Expands a SegmentTimeline into segment numbers (DASH: total = r + 1).
pub fn expand_timeline(timeline: &[TimelineS], start_number: i64) -> Vec<i64> {
    let mut out = Vec::new();
    let mut n = start_number;
    for s in timeline {
        let repeat = s.r.unwrap_or(0).max(0);
        for _ in 0..=repeat {
            out.push(n);
            n += 1;
        }
    }
    out
}

pub fn build_url(base: &str, rep_id: &str, template: &str, number: Option<i64>) -> String {
    let mut file = template.to_string();
    if let Some(n) = number {
        file = file.replace("$Number$", &format!("{n:05}"));
        file = file.replace("$Number%05d$", &format!("{n:05}"));
    }
    format!("{base}{}", file.replace("$RepresentationID$", rep_id))
}

/// Parses an inclusive "start-end" byte range.
pub fn parse_byte_range(r: &str) -> Result<(i64, i64)> {
    let (a, b) = r
        .split_once('-')
        .ok_or_else(|| anyhow!("invalid byte range {r:?}"))?;
    Ok((a.trim().parse()?, b.trim().parse()?))
}

/// Single-file representation, used by on-demand manifests.
#[derive(Debug, Clone)]
#[derive(Default)]
pub struct OnDemandRep {
    pub id: String,
    pub bandwidth: u64,
    pub height: u64,
    pub base_url: String,
    pub init_range: String,
    pub index_range: String,
}

#[derive(Debug, Clone)]
pub struct OnDemandSet {
    pub is_video: bool,
    pub reps: Vec<OnDemandRep>,
}

pub fn parse_on_demand_sets(mpd: &Mpd) -> Result<Vec<OnDemandSet>> {
    let period = mpd.first_period()?;
    Ok(period
        .adaptation_sets
        .iter()
        .map(|a| OnDemandSet {
            is_video: a.content_type.as_deref() == Some("video")
                || a.mime_type.as_deref().is_some_and(|m| m.starts_with("video/")),
            reps: a
                .representations
                .iter()
                .map(|r| {
                    let sb = r.segment_base.as_ref();
                    OnDemandRep {
                        id: r.id.clone(),
                        bandwidth: r.bandwidth.unwrap_or(0),
                        height: r.height.unwrap_or(0),
                        base_url: r.base_url.clone().unwrap_or_default(),
                        init_range: sb
                            .and_then(|s| s.initialization.as_ref())
                            .and_then(|i| i.range.clone())
                            .unwrap_or_default(),
                        index_range: sb.and_then(|s| s.index_range.clone()).unwrap_or_default(),
                    }
                })
                .collect(),
        })
        .collect())
}

/// Chooses a video rep by height, or an audio rep by bandwidth floor. Falls back
/// to the first representation, printing a notice (same as the Go version).
pub fn select_on_demand_rep(set: &OnDemandSet, quality: &str) -> Result<OnDemandRep> {
    let kind = if set.is_video { "video" } else { "audio" };
    let first = set
        .reps
        .first()
        .ok_or_else(|| anyhow!("no {kind} representation available"))?;
    if quality.eq_ignore_ascii_case("best") {
        let best = if set.is_video {
            set.reps.iter().max_by_key(|r| r.height)
        } else {
            set.reps.iter().max_by_key(|r| r.bandwidth)
        };
        if let Some(r) = best {
            return Ok(r.clone());
        }
    }
    if set.is_video {
        if let Ok(target) = quality.replace('p', "").parse::<u64>() {
            if let Some(r) = set.reps.iter().find(|r| r.height == target) {
                return Ok(r.clone());
            }
        }
    } else {
        let floor = match quality.replace('k', "").as_str() {
            "192" => 192_000,
            "128" => 128_000,
            "96" => 96_000,
            _ => u64::MAX,
        };
        if let Some(r) = set.reps.iter().find(|r| r.bandwidth >= floor) {
            return Ok(r.clone());
        }
    }
    let label = if set.is_video { "Video" } else { "Audio" };
    crate::say!("{label} quality {quality} not found, deferring to {}", first.id);
    Ok(first.clone())
}

/// Segmented-manifest rep: (base URL, rep id, template, timeline numbers).
pub struct SegmentedRep {
    pub base: String,
    pub rep_id: String,
    pub init_template: String,
    pub media_template: String,
    pub numbers: Vec<i64>,
}

pub fn select_segmented(set: &AdaptationSet, is_video: bool, quality: &str) -> Result<SegmentedRep> {
    let tmpl = set
        .segment_template
        .as_ref()
        .ok_or_else(|| anyhow!("adaptation set has no SegmentTemplate"))?;
    let chosen = if quality.eq_ignore_ascii_case("best") {
        if is_video {
            set.representations.iter().max_by_key(|r| r.height.unwrap_or(0))
        } else {
            set.representations.iter().max_by_key(|r| r.bandwidth.unwrap_or(0))
        }
    } else if is_video {
        let target = quality.replace('p', "").parse::<u64>().ok();
        set.representations.iter().find(|r| r.height.is_some() && r.height == target)
    } else {
        let floor = match quality.replace('k', "").as_str() {
            "192" => Some(192_000),
            "128" => Some(128_000),
            "96" => Some(96_000),
            _ => None,
        };
        set.representations.iter().find(|r| {
            if r.id.contains("audio/") {
                r.id.contains(quality)
            } else {
                matches!((r.bandwidth, floor), (Some(b), Some(f)) if b >= f)
            }
        })
    };
    let rep = match chosen {
        Some(r) => r,
        None => {
            let first = set
                .representations
                .first()
                .ok_or_else(|| anyhow!("no representation available"))?;
            crate::say!("Audio quality {quality} not found, deferring to {}", first.id);
            first
        }
    };
    let base = rep
        .base_url
        .clone()
        .ok_or_else(|| anyhow!("representation {} has no BaseURL", rep.id))?;
    let numbers = match &tmpl.timeline {
        Some(t) => expand_timeline(&t.s, 1),
        None => bail!("SegmentTemplate has no SegmentTimeline"),
    };
    Ok(SegmentedRep {
        base,
        rep_id: rep.id.clone(),
        init_template: tmpl.initialization.clone(),
        media_template: tmpl.media.clone(),
        numbers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeline_expansion() {
        let t = vec![TimelineS { r: Some(2) }, TimelineS { r: None }];
        assert_eq!(expand_timeline(&t, 1), vec![1, 2, 3, 4]);
    }

    #[test]
    fn byte_range() {
        assert_eq!(parse_byte_range("0-1837").unwrap(), (0, 1837));
        assert!(parse_byte_range("nope").is_err());
    }

    #[test]
    fn url_template() {
        assert_eq!(
            build_url("https://x/", "video/1", "seg-$RepresentationID$-$Number%05d$.m4s", Some(7)),
            "https://x/seg-video/1-00007.m4s"
        );
    }

    #[test]
    fn pssh_prefers_widevine() {
        let xml = r#"<MPD><Period><AdaptationSet>
            <ContentProtection schemeIdUri="urn:uuid:9a04f079-9840-4286-ab92-e65be0885f95"><cenc:pssh>AAA=</cenc:pssh></ContentProtection>
            <ContentProtection schemeIdUri="urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed"><cenc:pssh>BBB=</cenc:pssh></ContentProtection>
        </AdaptationSet></Period></MPD>"#;
        let m = Mpd::parse(xml).unwrap();
        assert_eq!(m.pssh().as_deref(), Some("BBB="));
    }

    /// Port of Go TestParseByteRange.
    #[test]
    fn parse_byte_range_cases() {
        for (input, want) in [
            ("0-1837", Some((0, 1837))),
            ("1838-6129", Some((1838, 6129))),
            ("1708-6011", Some((1708, 6011))),
            ("12345-", None),
            ("", None),
            ("abc-def", None),
        ] {
            match (parse_byte_range(input), want) {
                (Ok(got), Some(w)) => assert_eq!(got, w, "parse_byte_range({input:?})"),
                (Err(_), None) => {}
                (got, want) => panic!("parse_byte_range({input:?}) = {got:?}, want {want:?}"),
            }
        }
    }

    /// Port of Go TestParseOnDemand: two video reps and one audio rep.
    #[test]
    fn parse_on_demand_sample() {
        let body = r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" profiles="urn:mpeg:dash:profile:isoff-on-demand:2011" type="static">
  <Period>
    <AdaptationSet contentType="video" mimeType="video/mp4" segmentAlignment="true">
      <Representation id="84df326d" bandwidth="4386044" width="1280" height="720" codecs="avc1.64001F">
        <BaseURL>https://example.com/video.mp4</BaseURL>
        <SegmentBase indexRange="1838-6129">
          <Initialization range="0-1837"/>
        </SegmentBase>
      </Representation>
      <Representation id="b32ce5ea" bandwidth="16481538" width="1920" height="1080" codecs="avc1.640028">
        <BaseURL>https://example.com/video-1080.mp4</BaseURL>
        <SegmentBase indexRange="1839-6130">
          <Initialization range="0-1838"/>
        </SegmentBase>
      </Representation>
    </AdaptationSet>
    <AdaptationSet contentType="audio" mimeType="audio/mp4" segmentAlignment="true">
      <Representation id="433db54e" bandwidth="199094" codecs="mp4a.40.2" audioSamplingRate="48000">
        <BaseURL>https://example.com/audio.mp4</BaseURL>
        <SegmentBase indexRange="1708-6011">
          <Initialization range="0-1707"/>
        </SegmentBase>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let mpd = Mpd::parse(body).unwrap();
        let sets = parse_on_demand_sets(&mpd).unwrap();
        assert_eq!(sets.len(), 2);
        assert!(sets[0].is_video && !sets[1].is_video, "video flags");
        assert_eq!(sets[0].reps.len(), 2);
        let rep = &sets[0].reps[1];
        assert_eq!((rep.height, rep.init_range.as_str(), rep.index_range.as_str()), (1080, "0-1838", "1839-6130"));
        let audio = &sets[1].reps[0];
        assert_eq!(
            (audio.bandwidth, audio.init_range.as_str(), audio.index_range.as_str()),
            (199094, "0-1707", "1708-6011")
        );
    }

    /// Port of Go TestSelectOnDemandRepresentation.
    #[test]
    fn select_on_demand_representation() {
        let video = OnDemandSet {
            is_video: true,
            reps: vec![
                OnDemandRep { id: "720p".into(), height: 720, ..Default::default() },
                OnDemandRep { id: "1080p".into(), height: 1080, ..Default::default() },
            ],
        };
        assert_eq!(select_on_demand_rep(&video, "1080p").unwrap().id, "1080p");

        let audio = OnDemandSet {
            is_video: false,
            reps: vec![
                OnDemandRep { id: "128k".into(), bandwidth: 135_875, ..Default::default() },
                OnDemandRep { id: "192k".into(), bandwidth: 200_315, ..Default::default() },
            ],
        };
        assert_eq!(select_on_demand_rep(&audio, "192k").unwrap().id, "192k");

        // Unknown quality falls back to the first representation.
        assert_eq!(select_on_demand_rep(&video, "480p").unwrap().id, "720p");
    }
}
