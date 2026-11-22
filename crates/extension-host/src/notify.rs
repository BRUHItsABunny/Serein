//! Notification pipeline: every alert the host is about to show, from the gateway or (on
//! mobile) a push message, passes through the enabled plugins with a `notification` action,
//! one after another. Each may reword or hide it. Delivery fails open: a plugin that errors,
//! or a chain that has not answered by [`DEADLINE`], shows the alert as it stands.
//!
//! The same message from both sources alerts once. A push alert is replaced ("upgraded")
//! when its gateway copy arrives, and a plugin answering after the deadline upgrades the
//! alert it was too slow for, as long as no newer alert was shown since.
use extensions::{NotificationDecision, NotificationInput, NotificationSource};
use model::Id;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How long an alert waits for its plugins before the host shows it as it stands.
pub const DEADLINE: Duration = Duration::from_millis(1500);
/// How long a late answer may still upgrade the alert it was too slow for.
pub const LATE: Duration = Duration::from_secs(10);
/// Alerts waiting for plugins; more are shown without them.
const MAX_WAITING: usize = 16;
/// Messages remembered for deduplication across sources.
const MAX_SEEN: usize = 512;

/// An alert, with whatever the front end needs to show it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Alert<T> {
	pub source: NotificationSource,
	pub message: Id,
	pub channel: Id,
	pub guild: Option<Id>,
	pub author: Option<Id>,
	pub direct: bool,
	pub title: String,
	pub body: String,
	pub extra: T,
}

/// An alert ready to show. `upgrade` replaces the alert shown for the same message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ready<T> {
	pub alert: Alert<T>,
	pub upgrade: bool,
}

/// A plugin to run, with the alert it is given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
	pub plugin: String,
	pub action: String,
	pub input: NotificationInput,
}

struct Waiting<T> {
	alert: Alert<T>,
	/// The handler to run next, as an index into the handlers when this alert arrived.
	next: usize,
	handlers: Vec<(String, String)>,
	since: Instant,
	in_flight: Option<String>,
	/// Shown at the deadline; a late rewording upgrades it.
	shown: bool,
	upgrade: bool,
}

pub struct Pipeline<T> {
	handlers: Vec<(String, String)>,
	waiting: VecDeque<Waiting<T>>,
	ready: VecDeque<Ready<T>>,
	/// Recent messages with the source that alerted for them.
	seen: VecDeque<(Id, NotificationSource)>,
	/// The message whose alert was shown last.
	last_shown: Option<Id>,
}

impl<T> Default for Pipeline<T> {
	fn default() -> Self {
		Self {
			handlers: Vec::new(),
			waiting: VecDeque::new(),
			ready: VecDeque::new(),
			seen: VecDeque::new(),
			last_shown: None,
		}
	}
}

impl<T: Clone> Pipeline<T> {
	/// The enabled plugins with a `notification` action, as (plugin, action), in the order
	/// they run. Alerts already waiting keep the plugins they started with, minus removed ones.
	pub fn set_handlers(&mut self, handlers: Vec<(String, String)>) {
		for waiting in &mut self.waiting {
			let (done, rest) = waiting
				.handlers
				.split_at(waiting.next.min(waiting.handlers.len()));
			let mut kept = done.to_vec();
			let in_flight = waiting.in_flight.as_deref();
			kept.extend(rest.iter().enumerate().filter_map(|(index, handler)| {
				let current = index == 0 && in_flight == Some(handler.0.as_str());
				(current || handlers.contains(handler)).then(|| handler.clone())
			}));
			waiting.handlers = kept;
		}
		self.handlers = handlers;
	}
	pub fn has_handlers(&self) -> bool {
		!self.handlers.is_empty()
	}
	/// Takes in an alert. It is dropped when this message already alerted, unless this is
	/// the gateway copy of a push alert, which replaces it.
	pub fn offer(&mut self, alert: Alert<T>, now: Instant) {
		let mut upgrade = match self.seen.iter().find(|(id, _)| *id == alert.message) {
			Some((_, NotificationSource::Push)) if alert.source == NotificationSource::Gateway => {
				true
			}
			Some(_) => return,
			None => false,
		};
		if upgrade {
			// A push copy not shown yet is replaced before it shows, so nothing is upgraded.
			let before = self.waiting.len() + self.ready.len();
			self.waiting
				.retain(|waiting| waiting.alert.message != alert.message || waiting.shown);
			self.ready
				.retain(|ready| ready.alert.message != alert.message);
			if self.waiting.len() + self.ready.len() < before {
				upgrade = false;
			}
			// A late push copy still running is superseded too.
			self.waiting
				.retain(|waiting| waiting.alert.message != alert.message);
		}
		self.seen.retain(|(id, _)| *id != alert.message);
		if self.seen.len() >= MAX_SEEN {
			self.seen.pop_front();
		}
		self.seen.push_back((alert.message, alert.source));
		if self.handlers.is_empty() || self.waiting.len() >= MAX_WAITING {
			self.show(alert, upgrade);
			return;
		}
		self.waiting.push_back(Waiting {
			alert,
			next: 0,
			handlers: self.handlers.clone(),
			since: now,
			in_flight: None,
			shown: false,
			upgrade,
		});
	}
	fn show(&mut self, alert: Alert<T>, upgrade: bool) {
		// Upgrading an alert that is no longer the newest would cover the newer one.
		if upgrade && self.last_shown != Some(alert.message) {
			return;
		}
		self.last_shown = Some(alert.message);
		self.ready.push_back(Ready { alert, upgrade });
	}
	/// The next plugin to run, when none is running. Its answer or failure must come back
	/// through [`Self::answer`] or [`Self::failed`].
	pub fn next_request(&mut self) -> Option<Request> {
		if self
			.waiting
			.iter()
			.any(|waiting| waiting.in_flight.is_some())
		{
			return None;
		}
		let waiting = self
			.waiting
			.iter_mut()
			.find(|waiting| waiting.next < waiting.handlers.len())?;
		let (plugin, action) = waiting.handlers[waiting.next].clone();
		waiting.in_flight = Some(plugin.clone());
		let alert = &waiting.alert;
		Some(Request {
			plugin,
			action,
			input: NotificationInput {
				source: alert.source,
				message_id: alert.message.0.to_string(),
				channel_id: alert.channel.0.to_string(),
				guild_id: alert.guild.map(|id| id.0.to_string()),
				author_id: alert.author.map(|id| id.0.to_string()),
				direct: alert.direct,
				title: alert.title.clone(),
				body: alert.body.clone(),
				upgrade: waiting.shown || waiting.upgrade,
			},
		})
	}
	/// A plugin's answer for the alert of `message`.
	pub fn answer(&mut self, plugin: &str, message: &str, decision: Option<NotificationDecision>) {
		let Some(index) = self.in_flight(plugin, message) else {
			return;
		};
		let waiting = &mut self.waiting[index];
		waiting.in_flight = None;
		waiting.next += 1;
		let decision = decision.unwrap_or_default();
		if decision.hide {
			// An alert already shown cannot be taken back; the rest of the chain is skipped.
			self.waiting.remove(index);
			return;
		}
		let mut changed = false;
		if let Some(title) = decision.title {
			changed |= title != waiting.alert.title;
			waiting.alert.title = title;
		}
		if let Some(body) = decision.body {
			changed |= body != waiting.alert.body;
			waiting.alert.body = body;
		}
		if waiting.shown {
			let waiting = self.waiting.remove(index).expect("found above");
			if changed {
				self.show(waiting.alert, true);
			}
			return;
		}
		self.finish_if_done(index);
	}
	/// The plugin failed on the alert of `message`; the alert continues without it.
	pub fn failed(&mut self, plugin: &str, message: &str) {
		let Some(index) = self.in_flight(plugin, message) else {
			return;
		};
		let waiting = &mut self.waiting[index];
		waiting.in_flight = None;
		waiting.next += 1;
		if waiting.shown {
			self.waiting.remove(index);
			return;
		}
		self.finish_if_done(index);
	}
	fn in_flight(&self, plugin: &str, message: &str) -> Option<usize> {
		self.waiting.iter().position(|waiting| {
			waiting.in_flight.as_deref() == Some(plugin)
				&& waiting.alert.message.0.to_string() == message
		})
	}
	fn finish_if_done(&mut self, index: usize) {
		if self.waiting[index].next >= self.waiting[index].handlers.len() {
			let waiting = self.waiting.remove(index).expect("index is valid");
			self.show(waiting.alert, waiting.upgrade);
		}
	}
	/// Shows alerts whose plugins missed the deadline, and forgets late ones past [`LATE`].
	/// Returns when to call again, if anything is waiting.
	pub fn expire(&mut self, now: Instant) -> Option<Duration> {
		let mut index = 0;
		while index < self.waiting.len() {
			let waiting = &mut self.waiting[index];
			let age = now.saturating_duration_since(waiting.since);
			if !waiting.shown && age >= DEADLINE {
				waiting.shown = true;
				let (alert, upgrade) = (waiting.alert.clone(), waiting.upgrade);
				let running = waiting.in_flight.is_some();
				if !running {
					self.waiting.remove(index);
				}
				self.show(alert, upgrade);
				if !running {
					continue;
				}
			} else if self.waiting[index].shown && age >= LATE {
				// The answer never came; the request is no longer tracked.
				self.waiting.remove(index);
				continue;
			}
			index += 1;
		}
		self.waiting
			.iter()
			.map(|waiting| {
				let limit = if waiting.shown { LATE } else { DEADLINE };
				limit.saturating_sub(now.saturating_duration_since(waiting.since))
			})
			.min()
	}
	pub fn take_ready(&mut self) -> Option<Ready<T>> {
		self.ready.pop_front()
	}
	/// Drops waiting and late alerts, after a read, mute or account change made them stale.
	/// Deduplication history is kept unless `forget` is set.
	pub fn dismiss(&mut self, forget: bool) {
		self.waiting.clear();
		self.ready.clear();
		self.last_shown = None;
		if forget {
			self.seen.clear();
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn alert(message: u64, source: NotificationSource) -> Alert<()> {
		Alert {
			source,
			message: Id(message),
			channel: Id(20),
			guild: None,
			author: Some(Id(5)),
			direct: true,
			title: "Ada".into(),
			body: "hola".into(),
			extra: (),
		}
	}
	fn reword(body: &str) -> Option<NotificationDecision> {
		Some(NotificationDecision {
			body: Some(body.into()),
			..Default::default()
		})
	}

	/// Two plugins run in order on each alert; the second sees the first's rewording. One
	/// hides an alert, one fails and the alert shows unchanged, and a chain that misses the
	/// deadline shows the alert as it stands and upgrades it when the late answer arrives,
	/// unless a newer alert was shown meanwhile.
	#[test]
	fn plugins_shape_alerts_in_order_and_failures_or_slowness_fail_open() {
		let now = Instant::now();
		let mut pipeline = Pipeline::default();
		pipeline.set_handlers(vec![("a".into(), "n".into()), ("b".into(), "n".into())]);
		pipeline.offer(alert(1, NotificationSource::Gateway), now);
		pipeline.offer(alert(2, NotificationSource::Gateway), now);
		let first = pipeline.next_request().unwrap();
		assert_eq!(
			(first.plugin.as_str(), first.input.message_id.as_str()),
			("a", "1")
		);
		assert!(
			pipeline.next_request().is_none(),
			"one plugin runs at a time"
		);
		pipeline.answer("a", "1", reword("hello"));
		let second = pipeline.next_request().unwrap();
		assert_eq!(
			(second.plugin.as_str(), second.input.body.as_str()),
			("b", "hello")
		);
		// An answer from the wrong plugin, or for another message, changes nothing.
		pipeline.answer("a", "1", reword("x"));
		pipeline.answer("b", "2", reword("x"));
		pipeline.answer("b", "1", None);
		let shown = pipeline.take_ready().unwrap();
		assert_eq!((shown.alert.body.as_str(), shown.upgrade), ("hello", false));

		pipeline.next_request().unwrap();
		pipeline.answer(
			"a",
			"2",
			Some(NotificationDecision {
				hide: true,
				..Default::default()
			}),
		);
		assert!(pipeline.next_request().is_none() && pipeline.take_ready().is_none());

		pipeline.offer(alert(3, NotificationSource::Gateway), now);
		pipeline.next_request().unwrap();
		pipeline.failed("a", "3");
		pipeline.next_request().unwrap();
		pipeline.failed("b", "3");
		assert_eq!(pipeline.take_ready().unwrap().alert.body, "hola");

		// Plugin a is slow: the alert shows at the deadline, and its answer upgrades it.
		pipeline.offer(alert(4, NotificationSource::Gateway), now);
		pipeline.next_request().unwrap();
		assert_eq!(
			pipeline.expire(now + Duration::from_millis(10)),
			Some(DEADLINE - Duration::from_millis(10))
		);
		pipeline.expire(now + DEADLINE);
		assert!(!pipeline.take_ready().unwrap().upgrade);
		pipeline.answer("a", "4", reword("hello"));
		let late = pipeline.take_ready().unwrap();
		assert_eq!((late.alert.body.as_str(), late.upgrade), ("hello", true));
		assert!(
			pipeline.next_request().is_none(),
			"the rest of a late chain is skipped"
		);

		// A late answer after a newer alert was shown does not cover it.
		pipeline.offer(alert(5, NotificationSource::Gateway), now);
		pipeline.next_request().unwrap();
		pipeline.expire(now + DEADLINE);
		pipeline.take_ready().unwrap();
		pipeline.set_handlers(Vec::new());
		pipeline.offer(alert(6, NotificationSource::Gateway), now);
		assert_eq!(pipeline.take_ready().unwrap().alert.message, Id(6));
		pipeline.answer("a", "5", reword("late"));
		assert!(pipeline.take_ready().is_none());
	}

	/// A message alerts once whichever source reports it first, and the gateway copy of a
	/// push alert replaces it, including one still waiting for plugins.
	#[test]
	fn the_same_message_alerts_once_and_gateway_detail_upgrades_a_push_alert() {
		let now = Instant::now();
		let mut pipeline = Pipeline::default();
		pipeline.offer(alert(1, NotificationSource::Gateway), now);
		pipeline.offer(alert(1, NotificationSource::Push), now);
		pipeline.offer(alert(1, NotificationSource::Gateway), now);
		assert!(!pipeline.take_ready().unwrap().upgrade);
		assert!(pipeline.take_ready().is_none());

		pipeline.offer(alert(2, NotificationSource::Push), now);
		assert!(!pipeline.take_ready().unwrap().upgrade);
		let mut detailed = alert(2, NotificationSource::Gateway);
		detailed.body = "full text".into();
		pipeline.offer(detailed, now);
		let upgraded = pipeline.take_ready().unwrap();
		assert_eq!(
			(upgraded.alert.body.as_str(), upgraded.upgrade),
			("full text", true)
		);
		pipeline.offer(alert(2, NotificationSource::Gateway), now);
		assert!(pipeline.take_ready().is_none(), "upgrades happen once");

		// A push alert still waiting for its plugin is replaced by its gateway copy.
		pipeline.set_handlers(vec![("a".into(), "n".into())]);
		pipeline.offer(alert(3, NotificationSource::Push), now);
		pipeline.next_request().unwrap();
		pipeline.offer(alert(3, NotificationSource::Gateway), now);
		pipeline.answer("a", "3", None);
		assert!(
			pipeline.take_ready().is_none(),
			"the push copy's answer is ignored"
		);
		let request = pipeline.next_request().unwrap();
		assert_eq!(request.input.source, NotificationSource::Gateway);
		pipeline.answer("a", "3", None);
		assert_eq!(
			pipeline.take_ready().unwrap().alert.source,
			NotificationSource::Gateway
		);
	}
}
