//! Smart Shuffle: the shuffled context with songs Spotify suggests for it
//! woven in, one after every [SMART_SHUFFLE_INTERVAL] of the context's own.
//!
//! Spotify's apps switch it with the `context_enhancement` player mode,
//! `RECOMMENDATION` or `NONE`, beside `shuffling_context`, and leave the
//! player to fill it in. The suggestions come from the autoplay context, the
//! request that also continues a finished context.

use crate::{
    core::Error,
    protocol::{
        autoplay_context_request::AutoplayContextRequest, context::Context, player::ProvidedTrack,
    },
    state::{
        ConnectState,
        context::{ContextType, StateContext},
        metadata::Metadata,
        provider::IsProvider,
    },
};
use rand::Rng;
use std::collections::HashSet;

/// The player mode Spotify's apps set for Smart Shuffle.
pub const CONTEXT_ENHANCEMENT: &str = "context_enhancement";
const RECOMMENDATION: &str = "RECOMMENDATION";
const NONE: &str = "NONE";

/// Songs of the context between two suggestions.
pub const SMART_SHUFFLE_INTERVAL: usize = 3;
/// Suggestions held ready beyond the slots the next tracks have reached.
const LOOKAHEAD: usize = 8;
/// Songs of the context a request bases its suggestions on.
const SEEDS: usize = 5;

/// Whether a `context_enhancement` mode value asks for Smart Shuffle.
pub fn is_recommendation(mode: &str) -> bool {
    mode == RECOMMENDATION
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Suggestions {
    /// The context they were suggested for.
    context_uri: String,
    /// Slot `k` plays before the context's song at shuffled position
    /// `(k + 1) * SMART_SHUFFLE_INTERVAL`.
    tracks: Vec<ProvidedTrack>,
    /// Uids of suggestions that have played, or are playing.
    played: HashSet<String>,
    /// Changes whenever `tracks` does.
    revision: u64,
}

impl ConnectState {
    /// Smart Shuffle is on: shuffle, plus the recommendation mode.
    pub fn smart_shuffle(&self) -> bool {
        self.shuffling_context()
            && self
                .mode(CONTEXT_ENHANCEMENT)
                .is_some_and(is_recommendation)
    }

    /// Reports Smart Shuffle on or off, without touching the next tracks.
    pub fn set_smart_shuffle(&mut self, smart_shuffle: bool) {
        let mode = if smart_shuffle { RECOMMENDATION } else { NONE };
        self.merge_modes([(CONTEXT_ENHANCEMENT.to_owned(), mode.to_owned())]);
    }

    /// Switches Smart Shuffle on, shuffling the context if it isn't yet, or
    /// off, which keeps shuffle on and takes the suggestions out. They are
    /// woven in as [ConnectState::add_suggestions] receives them.
    pub fn handle_smart_shuffle(&mut self, smart_shuffle: bool) -> Result<(), Error> {
        if smart_shuffle == self.smart_shuffle() {
            self.set_smart_shuffle(smart_shuffle);
            return Ok(());
        }

        self.set_smart_shuffle(smart_shuffle);
        if !smart_shuffle {
            return self.drop_suggestions();
        }
        if !self.shuffling_context() {
            self.handle_shuffle(true)?;
        }
        Ok(())
    }

    /// Forgets the suggestions and takes them out of the next tracks.
    pub fn drop_suggestions(&mut self) -> Result<(), Error> {
        let revision = self.suggestions.revision + 1;
        self.suggestions = Suggestions {
            revision,
            ..Default::default()
        };

        let before = self.next_tracks().len();
        self.next_tracks_mut().retain(|t| !t.is_suggestion());
        if self.next_tracks().len() != before && self.context.is_some() {
            self.fill_up_next_tracks()?;
        }
        Ok(())
    }

    /// The suggestions for the current context, in the order they play.
    pub fn suggestion_uris(&self) -> Vec<String> {
        if self.suggestions.context_uri != *self.context_uri() {
            return Vec::new();
        }
        self.suggestions
            .tracks
            .iter()
            .map(|t| t.uri.clone())
            .collect()
    }

    /// Changes whenever the suggestions do.
    pub fn suggestions_revision(&self) -> u64 {
        self.suggestions.revision
    }

    /// Whether Smart Shuffle wants more suggestions than it holds for the
    /// slots coming up.
    pub fn needs_suggestions(&self) -> bool {
        if !self.smart_shuffle() {
            return false;
        }
        let Ok(ctx) = self.get_context(ContextType::Default) else {
            return false;
        };
        if self.suggestions.context_uri != *self.context_uri() {
            return true;
        }
        let slots = ctx.tracks.len().saturating_sub(1) / SMART_SHUFFLE_INTERVAL;
        let reached = ctx.index.track as usize / SMART_SHUFFLE_INTERVAL;
        self.suggestions.tracks.len() < slots.min(reached + LOOKAHEAD)
    }

    /// The autoplay request for more suggestions, based on the context and a
    /// few of its songs picked at random, so that each request brings others.
    /// `None` for contexts Spotify suggests nothing for, such as podcasts.
    pub fn suggestion_request(&self) -> Option<AutoplayContextRequest> {
        let context_uri = ConnectState::valid_resolve_uri(self.context_uri())?;
        if context_uri.contains("spotify:show:") || context_uri.contains("spotify:episode:") {
            return None;
        }
        let ctx = self.get_context(ContextType::Default).ok()?;
        let songs: Vec<&str> = ctx
            .tracks
            .iter()
            .filter(|t| t.uri.starts_with("spotify:track:") && !t.is_suggestion())
            .map(|t| t.uri.as_str())
            .collect();
        if songs.is_empty() {
            return None;
        }
        let mut rng = rand::rng();
        let recent_track_uri = (0..SEEDS.min(songs.len()))
            .map(|_| songs[rng.random_range(0..songs.len())].to_owned())
            .collect();
        Some(AutoplayContextRequest {
            context_uri: Some(context_uri.to_owned()),
            recent_track_uri,
            ..Default::default()
        })
    }

    /// Takes the songs of an autoplay answer for `context_uri` that are new,
    /// neither in the context nor suggested already, and weaves them into
    /// the next tracks. Returns how many it took.
    pub fn add_suggestions(&mut self, context_uri: &str, answer: Context) -> Result<usize, Error> {
        if !self.smart_shuffle() || context_uri != self.context_uri() {
            return Ok(0);
        }
        if self.suggestions.context_uri != context_uri {
            let revision = self.suggestions.revision + 1;
            self.suggestions = Suggestions {
                context_uri: context_uri.to_owned(),
                revision,
                ..Default::default()
            };
        }

        let ctx = self.get_context(ContextType::Default)?;
        let mut known: HashSet<String> = ctx
            .tracks
            .iter()
            .chain(&self.suggestions.tracks)
            .map(|t| t.uri.clone())
            .collect();
        let mut new = Vec::new();
        for track in answer.pages.iter().flat_map(|page| &page.tracks) {
            let Ok(mut track) =
                self.context_to_provided_track(track, Some(context_uri), None, None, None)
            else {
                continue;
            };
            if !track.uri.starts_with("spotify:track:")
                || track.is_unavailable()
                || !known.insert(track.uri.clone())
            {
                continue;
            }
            track.set_suggestion(true);
            new.push(track);
        }

        let added = new.len();
        if added > 0 {
            self.suggestions.tracks.extend(new);
            self.suggestions.revision += 1;
            self.weave_suggestions()?;
        }
        Ok(added)
    }

    /// The suggestion to play before the context's song at `position`, if
    /// one is due there and hasn't played or been put in the next tracks.
    pub(super) fn suggestion_before(
        &self,
        position: usize,
        iteration: u32,
    ) -> Option<ProvidedTrack> {
        if !matches!(self.fill_up_context, ContextType::Default)
            || iteration != 0
            || position == 0
            || position % SMART_SHUFFLE_INTERVAL != 0
            || !self.smart_shuffle()
            || self.suggestions.context_uri != *self.context_uri()
        {
            return None;
        }
        let ctx = self.get_context(ContextType::Default).ok()?;
        if position >= ctx.tracks.len() {
            return None;
        }
        let suggestion = self
            .suggestions
            .tracks
            .get(position / SMART_SHUFFLE_INTERVAL - 1)?;
        let waiting = self.next_tracks().iter().any(|t| t.uid == suggestion.uid);
        (!waiting && !self.suggestions.played.contains(&suggestion.uid)).then(|| suggestion.clone())
    }

    /// Notes that a suggestion is playing, so that it isn't woven in again.
    pub(super) fn suggestion_played(&mut self, track: &ProvidedTrack) {
        if track.is_suggestion() {
            self.suggestions.played.insert(track.uid.clone());
        }
    }

    /// Fills the next tracks again from where the context continues, so that
    /// suggestions that just arrived take their slots. Rows you queued stay.
    fn weave_suggestions(&mut self) -> Result<(), Error> {
        if !matches!(self.fill_up_context, ContextType::Default) {
            return Ok(());
        }
        let ctx = self.get_context(ContextType::Default)?;
        let Some(from) = self
            .next_tracks()
            .iter()
            .find(|t| !t.is_queue())
            .and_then(|t| self.shuffled_position(ctx, t))
        else {
            return Ok(());
        };

        self.clear_next_tracks();
        self.update_context_index(ContextType::Default, from)?;
        self.fill_up_next_tracks()
    }

    /// Where a next track sits in the shuffled context: a suggestion at the
    /// position it plays before.
    fn shuffled_position(&self, ctx: &StateContext, track: &ProvidedTrack) -> Option<usize> {
        if track.is_suggestion() {
            let slot = self
                .suggestions
                .tracks
                .iter()
                .position(|s| s.uid == track.uid)?;
            return Some((slot + 1) * SMART_SHUFFLE_INTERVAL);
        }
        ctx.tracks.iter().position(|c| c.uid == track.uid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ConnectConfig,
        core::Session,
        protocol::{context_page::ContextPage, context_track::ContextTrack},
    };

    fn state() -> ConnectState {
        let session = Session::new(Default::default(), None);
        ConnectState::new(ConnectConfig::default(), &session)
    }

    fn context(uri: &str, prefix: &str, len: usize) -> Context {
        Context {
            uri: Some(uri.into()),
            pages: vec![ContextPage {
                tracks: (0..len)
                    .map(|i| ContextTrack {
                        uri: Some(format!(
                            "spotify:track:{prefix}{i:0>width$}",
                            width = 22 - prefix.len()
                        )),
                        uid: Some(format!("{prefix}-uid-{i}")),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    const PLAYLIST: &str = "spotify:playlist:smart";

    fn playing(len: usize) -> ConnectState {
        let mut state = state();
        state
            .update_context(context(PLAYLIST, "1", len), ContextType::Default)
            .unwrap();
        state.set_current_track(0).unwrap();
        state.reset_playback_to_position(Some(0)).unwrap();
        state
    }

    fn next_uris(state: &ConnectState) -> Vec<String> {
        state.next_tracks().iter().map(|t| t.uri.clone()).collect()
    }

    #[tokio::test]
    async fn suggestions_play_one_after_every_three_songs() {
        let mut state = playing(30);
        state.handle_smart_shuffle(true).unwrap();
        assert!(state.smart_shuffle() && state.shuffling_context());
        assert_eq!(state.mode(CONTEXT_ENHANCEMENT), Some("RECOMMENDATION"));
        assert!(state.needs_suggestions());

        let added = state
            .add_suggestions(PLAYLIST, context("spotify:station:x", "2", 20))
            .unwrap();
        assert_eq!(added, 20);

        // the current song is position 0, so the queue starts at position 1:
        // 1 2 [s0] 3 4 5 [s1] 6 ...
        let next = state.next_tracks();
        let suggested: Vec<usize> = next
            .iter()
            .enumerate()
            .filter(|(_, t)| t.is_suggestion())
            .map(|(i, _)| i)
            .collect();
        assert_eq!(suggested[..3], [2, 6, 10]);
        // 29 songs follow the current one; positions 3..=27 hold 9 slots
        assert_eq!(suggested.len(), 9);
        assert_eq!(next.len(), 29 + 9);
        assert_eq!(state.suggestion_uris().len(), 20);
    }

    #[tokio::test]
    async fn suggestions_skip_songs_the_context_has_and_repeats() {
        let mut state = playing(12);
        state.handle_smart_shuffle(true).unwrap();
        let mut answer = context("spotify:station:x", "1", 4);
        answer.pages[0]
            .tracks
            .extend(context("spotify:station:x", "2", 2).pages.remove(0).tracks);
        assert_eq!(state.add_suggestions(PLAYLIST, answer.clone()).unwrap(), 2);
        assert_eq!(state.add_suggestions(PLAYLIST, answer).unwrap(), 0);
    }

    #[tokio::test]
    async fn a_playing_suggestion_keeps_the_context_going_and_is_not_woven_again() {
        let mut state = playing(30);
        state.handle_smart_shuffle(true).unwrap();
        state
            .add_suggestions(PLAYLIST, context("spotify:station:x", "2", 20))
            .unwrap();
        let after_first = state.next_tracks()[3].uri.clone();

        for _ in 0..3 {
            state.next_track().unwrap();
        }
        assert!(state.current_track(|t| t.is_suggestion()));
        assert_eq!(state.next_tracks()[0].uri, after_first);

        // more suggestions arrive while one plays: it doesn't come back
        state
            .add_suggestions(PLAYLIST, context("spotify:station:x", "3", 5))
            .unwrap();
        let uri = state.current_track(|t| t.uri.clone());
        assert!(!next_uris(&state).contains(&uri));
        assert_eq!(state.next_tracks()[0].uri, after_first);
    }

    #[tokio::test]
    async fn queued_songs_stay_on_top_when_suggestions_arrive() {
        let mut state = playing(30);
        state.handle_smart_shuffle(true).unwrap();
        state.add_to_queue(
            ProvidedTrack {
                uri: "spotify:track:queuedqueuedqueuedqueu".into(),
                ..Default::default()
            },
            true,
        );
        state
            .add_suggestions(PLAYLIST, context("spotify:station:x", "2", 20))
            .unwrap();
        assert!(state.next_tracks()[0].is_queue());
        assert!(state.next_tracks()[3].is_suggestion());
    }

    #[tokio::test]
    async fn switching_off_keeps_shuffle_and_takes_the_suggestions_out() {
        let mut state = playing(30);
        state.handle_smart_shuffle(true).unwrap();
        state
            .add_suggestions(PLAYLIST, context("spotify:station:x", "2", 20))
            .unwrap();
        state.handle_smart_shuffle(false).unwrap();
        assert!(state.shuffling_context());
        assert!(!state.smart_shuffle());
        assert_eq!(state.mode(CONTEXT_ENHANCEMENT), Some("NONE"));
        assert!(state.next_tracks().iter().all(|t| !t.is_suggestion()));
        assert_eq!(state.next_tracks().len(), 29);
        assert!(state.suggestion_uris().is_empty());
    }

    #[tokio::test]
    async fn unshuffling_during_a_suggestion_ends_smart_shuffle_and_keeps_playing_it() {
        let mut state = playing(30);
        state.handle_smart_shuffle(true).unwrap();
        state
            .add_suggestions(PLAYLIST, context("spotify:station:x", "2", 20))
            .unwrap();
        for _ in 0..3 {
            state.next_track().unwrap();
        }
        let suggestion = state.current_track(|t| t.uri.clone());
        state.handle_shuffle(false).unwrap();
        assert!(!state.smart_shuffle());
        assert_eq!(state.current_track(|t| t.uri.clone()), suggestion);
        assert!(state.next_tracks().iter().all(|t| !t.is_suggestion()));
    }

    #[tokio::test]
    async fn suggestions_for_another_context_are_ignored() {
        let mut state = playing(30);
        state.handle_smart_shuffle(true).unwrap();
        let added = state
            .add_suggestions(
                "spotify:playlist:other",
                context("spotify:station:x", "2", 20),
            )
            .unwrap();
        assert_eq!(added, 0);
        assert!(state.needs_suggestions());
    }

    #[tokio::test]
    async fn enough_suggestions_stop_the_requests() {
        let mut state = playing(30);
        state.handle_smart_shuffle(true).unwrap();
        state
            .add_suggestions(PLAYLIST, context("spotify:station:x", "2", 9))
            .unwrap();
        assert!(!state.needs_suggestions(), "30 songs have 9 slots");
        assert!(state.suggestion_request().is_some());
    }
}
