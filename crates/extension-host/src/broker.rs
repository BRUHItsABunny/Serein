//! Broadcasts between plugins.
//!
//! A provider broadcasts on a topic its manifest declares. Only subscriptions the user
//! approved for that exact subscriber, provider and topic receive a copy; with none, the
//! broadcast is dropped before anything is copied. Each subscriber has one bounded queue in
//! memory, so messages from one provider keep their order. Nothing is persisted or replayed:
//! a subscriber connected later never sees earlier messages, and logout, a scope change or
//! a revoked subscription discards what is queued.
//!
//! A full queue never drops silently: the messages it refuses become a `gap` marker in place,
//! counting what was lost. A subscriber's `topic` action reads messages with
//! `serein.topic_read`, and they leave its queue only once the host accepts the invocation,
//! so a failed delivery is retried with the next one.
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

use extensions::Reply;
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Messages one subscriber's queue holds.
pub const MAX_QUEUE_MESSAGES: usize = 64;
/// Payload bytes one subscriber's queue holds.
pub const MAX_QUEUE_BYTES: usize = 256 * 1024;
/// Gap markers a full queue adds beyond its message limit; later losses join the last one.
const MAX_GAPS: usize = 16;
/// Broadcasts one invocation may send.
pub const MAX_BROADCASTS: u32 = 32;
/// Reply budget of one `topic_read`, below the 256 KiB pending-reply limit.
const MAX_READ_BYTES: usize = 192 * 1024;

/// One approved connection: `subscriber` receives `topic` from `provider`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Subscription {
	pub subscriber: String,
	pub provider: String,
	pub topic: String,
}

enum Item {
	Message {
		seq: u64,
		provider: String,
		topic: String,
		at_ms: i64,
		payload: String,
	},
	Gap {
		seq: u64,
		provider: String,
		topic: String,
		dropped: u64,
	},
}

impl Item {
	fn seq(&self) -> u64 {
		match self {
			Self::Message { seq, .. } | Self::Gap { seq, .. } => *seq,
		}
	}
	fn bytes(&self) -> usize {
		match self {
			Self::Message { payload, .. } => payload.len(),
			Self::Gap { .. } => 0,
		}
	}
	fn from(&self, subscription: &Subscription) -> bool {
		let (provider, topic) = match self {
			Self::Message {
				provider, topic, ..
			}
			| Self::Gap {
				provider, topic, ..
			} => (provider, topic),
		};
		*provider == subscription.provider && *topic == subscription.topic
	}
	fn json(&self) -> String {
		match self {
			Self::Message {
				seq,
				provider,
				topic,
				at_ms,
				payload,
			} => format!(
				r#"{{"kind":"message","seq":{seq},"provider":{},"topic":{},"at_ms":{at_ms},"payload":{payload}}}"#,
				json!(provider),
				json!(topic)
			),
			Self::Gap {
				seq,
				provider,
				topic,
				dropped,
			} => json!({
				"kind": "gap",
				"seq": seq,
				"provider": provider,
				"topic": topic,
				"dropped": dropped,
			})
			.to_string(),
		}
	}
}

#[derive(Default)]
struct Queue {
	items: VecDeque<Item>,
	bytes: usize,
}

impl Queue {
	fn messages(&self) -> usize {
		self.items
			.iter()
			.filter(|item| matches!(item, Item::Message { .. }))
			.count()
	}
}

#[derive(Default)]
struct Inner {
	subscriptions: BTreeSet<Subscription>,
	queues: BTreeMap<String, Queue>,
	next_seq: u64,
}

/// The broker of one extension host; it holds only the current scope's subscriptions.
#[derive(Default)]
pub struct Broker(Mutex<Inner>);

fn now_ms() -> i64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map_or(0, |elapsed| elapsed.as_millis() as i64)
}

impl Broker {
	/// Replaces the approved subscriptions. Queued items of connections that are gone are
	/// discarded, and so are the queues of subscribers left without any.
	pub fn set_subscriptions(&self, subscriptions: BTreeSet<Subscription>) {
		let Ok(mut inner) = self.0.lock() else {
			return;
		};
		for (subscriber, queue) in &mut inner.queues {
			queue.items.retain(|item| {
				subscriptions.iter().any(|subscription| {
					subscription.subscriber == *subscriber && item.from(subscription)
				})
			});
			queue.bytes = queue.items.iter().map(Item::bytes).sum();
		}
		inner.queues.retain(|subscriber, _| {
			subscriptions
				.iter()
				.any(|subscription| subscription.subscriber == *subscriber)
		});
		inner.subscriptions = subscriptions;
	}

	pub fn has_subscribers(&self, provider: &str, topic: &str) -> bool {
		self.0.lock().is_ok_and(|inner| {
			inner.subscriptions.iter().any(|subscription| {
				subscription.provider == provider && subscription.topic == topic
			})
		})
	}

	/// Copies `payload` (serialized JSON) to every approved subscriber; returns how many
	/// queued it. A full queue records the loss as a gap instead.
	pub fn broadcast(&self, provider: &str, topic: &str, payload: &str) -> usize {
		let Ok(mut inner) = self.0.lock() else {
			return 0;
		};
		let subscribers: Vec<String> = inner
			.subscriptions
			.iter()
			.filter(|subscription| subscription.provider == provider && subscription.topic == topic)
			.map(|subscription| subscription.subscriber.clone())
			.collect();
		let mut reached = 0;
		for subscriber in subscribers {
			inner.next_seq += 1;
			let seq = inner.next_seq;
			let queue = inner.queues.entry(subscriber).or_default();
			if queue.messages() < MAX_QUEUE_MESSAGES
				&& queue.bytes + payload.len() <= MAX_QUEUE_BYTES
			{
				queue.bytes += payload.len();
				queue.items.push_back(Item::Message {
					seq,
					provider: provider.to_owned(),
					topic: topic.to_owned(),
					at_ms: now_ms(),
					payload: payload.to_owned(),
				});
				reached += 1;
				continue;
			}
			let gaps = queue.items.len() - queue.messages();
			match queue.items.back_mut() {
				// Consecutive losses of one connection, or any loss once gaps are at their
				// limit, extend the newest gap.
				Some(Item::Gap {
					provider: last_provider,
					topic: last_topic,
					dropped,
					..
				}) if (*last_provider == provider && *last_topic == topic) || gaps >= MAX_GAPS => {
					*dropped += 1;
				}
				_ if gaps >= MAX_GAPS => {}
				_ => queue.items.push_back(Item::Gap {
					seq,
					provider: provider.to_owned(),
					topic: topic.to_owned(),
					dropped: 1,
				}),
			}
		}
		reached
	}

	/// Subscribers with queued items, in a stable order.
	pub fn pending(&self) -> Vec<String> {
		self.0.lock().map_or_else(
			|_| Vec::new(),
			|inner| {
				inner
					.queues
					.iter()
					.filter(|(_, queue)| !queue.items.is_empty())
					.map(|(subscriber, _)| subscriber.clone())
					.collect()
			},
		)
	}

	/// Up to `max` items after `after` for `subscriber`, as reply JSON, with the last
	/// sequence read and whether more remain.
	fn read(&self, subscriber: &str, after: u64, max: u32) -> (Vec<u8>, u64, bool) {
		let Ok(inner) = self.0.lock() else {
			return (br#"{"messages":[],"more":false}"#.to_vec(), after, false);
		};
		let mut out = br#"{"messages":["#.to_vec();
		let mut last = after;
		let mut more = false;
		for (count, item) in inner
			.queues
			.get(subscriber)
			.into_iter()
			.flat_map(|queue| queue.items.iter())
			.filter(|item| item.seq() > after)
			.enumerate()
		{
			let json = item.json();
			if count >= max as usize || (count > 0 && out.len() + json.len() + 1 > MAX_READ_BYTES) {
				more = true;
				break;
			}
			if count > 0 {
				out.push(b',');
			}
			out.extend_from_slice(json.as_bytes());
			last = item.seq();
		}
		out.extend_from_slice(format!(r#"],"more":{more}}}"#).as_bytes());
		(out, last, more)
	}

	/// Removes `subscriber`'s items up to sequence `through`.
	fn acknowledge(&self, subscriber: &str, through: u64) {
		let Ok(mut inner) = self.0.lock() else {
			return;
		};
		if let Some(queue) = inner.queues.get_mut(subscriber) {
			while queue
				.items
				.front()
				.is_some_and(|item| item.seq() <= through)
			{
				let item = queue.items.pop_front().expect("checked above");
				queue.bytes -= item.bytes();
			}
		}
	}
}

/// One invocation's topic access for one plugin.
pub struct TopicSession {
	broker: Arc<Broker>,
	plugin: String,
	broadcasts: Vec<String>,
	subscribes: bool,
	sent: u32,
	/// Last sequence this invocation read, until the host accepts it.
	read: u64,
	more: bool,
}

impl TopicSession {
	pub(crate) fn new(
		broker: Arc<Broker>,
		plugin: String,
		broadcasts: Vec<String>,
		subscribes: bool,
	) -> Self {
		Self {
			broker,
			plugin,
			broadcasts,
			subscribes,
			sent: 0,
			read: 0,
			more: false,
		}
	}

	/// Whether the last read left messages for another delivery.
	pub fn more(&self) -> bool {
		self.more
	}

	pub fn broadcast(&mut self, request: &[u8]) -> i32 {
		use extensions::imports::status;
		#[derive(Deserialize)]
		#[serde(deny_unknown_fields)]
		struct Request {
			topic: String,
			payload: Box<serde_json::value::RawValue>,
		}
		let Ok(request) = serde_json::from_slice::<Request>(request) else {
			return status::INVALID;
		};
		if !self.broadcasts.contains(&request.topic)
			|| request.payload.get().len() > extensions::topics::MAX_PAYLOAD_BYTES
		{
			return status::INVALID;
		}
		if self.sent >= MAX_BROADCASTS {
			return status::UNAVAILABLE;
		}
		self.sent += 1;
		self.broker
			.broadcast(&self.plugin, &request.topic, request.payload.get()) as i32
	}

	pub fn has_subscribers(&self, topic: &str) -> i32 {
		if !self.broadcasts.iter().any(|declared| declared == topic) {
			return extensions::imports::status::INVALID;
		}
		i32::from(self.broker.has_subscribers(&self.plugin, topic))
	}

	pub fn read(&mut self, max: u32) -> Reply {
		if !self.subscribes {
			return Reply::Unavailable;
		}
		let (bytes, last, more) = self.broker.read(&self.plugin, self.read, max);
		self.read = last;
		self.more = more;
		Reply::Ok(bytes)
	}

	/// Removes what this invocation read; called once the host accepts it.
	pub fn commit(&mut self) {
		if self.read > 0 {
			self.broker.acknowledge(&self.plugin, self.read);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn subscription(subscriber: &str, provider: &str, topic: &str) -> Subscription {
		Subscription {
			subscriber: subscriber.into(),
			provider: provider.into(),
			topic: topic.into(),
		}
	}

	fn session(broker: &Arc<Broker>, plugin: &str) -> TopicSession {
		TopicSession::new(
			broker.clone(),
			plugin.into(),
			vec!["serein.captions/1".into()],
			true,
		)
	}

	fn read(session: &mut TopicSession, max: u32) -> serde_json::Value {
		let Reply::Ok(bytes) = session.read(max) else {
			panic!("read failed")
		};
		serde_json::from_slice(&bytes).unwrap()
	}

	#[test]
	fn only_approved_connections_receive_and_delivery_commits_with_the_invocation() {
		let broker = Arc::new(Broker::default());
		let mut captioner = session(&broker, "captioner");
		let broadcast = |session: &mut TopicSession, text: &str| {
			session.broadcast(
				json!({ "topic": "serein.captions/1", "payload": { "text": text } })
					.to_string()
					.as_bytes(),
			)
		};
		// Nobody listens: dropped before copying.
		assert_eq!(captioner.has_subscribers("serein.captions/1"), 0);
		assert_eq!(broadcast(&mut captioner, "lost"), 0);
		broker
			.set_subscriptions([subscription("recorder", "captioner", "serein.captions/1")].into());
		assert_eq!(captioner.has_subscribers("serein.captions/1"), 1);
		// Undeclared topics are refused.
		assert!(captioner.broadcast(br#"{"topic":"serein.other/1","payload":1}"#) < 0);
		assert_eq!(captioner.has_subscribers("serein.other/1"), -1);
		assert_eq!(broadcast(&mut captioner, "one"), 1);
		assert_eq!(broadcast(&mut captioner, "two"), 1);
		// Another provider on the same topic is not approved for this subscriber.
		assert_eq!(broadcast(&mut session(&broker, "stranger"), "spoofed"), 0);
		assert_eq!(broker.pending(), ["recorder"]);

		let mut recorder = session(&broker, "recorder");
		let batch = read(&mut recorder, 1);
		assert_eq!(batch["messages"][0]["payload"]["text"], "one");
		assert_eq!(batch["messages"][0]["provider"], "captioner");
		assert_eq!(batch["more"], true);
		// Not committed: the next invocation reads it again.
		drop(recorder);
		let mut recorder = session(&broker, "recorder");
		let batch = read(&mut recorder, 64);
		assert_eq!(batch["messages"].as_array().unwrap().len(), 2);
		recorder.commit();
		assert!(broker.pending().is_empty());
	}

	#[test]
	fn full_queues_record_gaps_and_revoked_connections_discard_messages() {
		let broker = Arc::new(Broker::default());
		broker
			.set_subscriptions([subscription("recorder", "captioner", "serein.captions/1")].into());
		for index in 0..MAX_QUEUE_MESSAGES + 5 {
			broker.broadcast("captioner", "serein.captions/1", &index.to_string());
		}
		let mut recorder = session(&broker, "recorder");
		let batch = read(&mut recorder, 64);
		assert_eq!(batch["messages"].as_array().unwrap().len(), 64);
		let rest = read(&mut recorder, 64);
		assert_eq!(rest["messages"][0]["kind"], "gap");
		assert_eq!(rest["messages"][0]["dropped"], 5);
		// Revoking the subscription discards what was queued.
		broker.set_subscriptions(BTreeSet::new());
		assert!(broker.pending().is_empty());
		assert_eq!(broker.broadcast("captioner", "serein.captions/1", "1"), 0);
	}
}
