# Extension topics (preview)

Topics let one plugin pass data to another without either knowing the other's code. The
host treats payloads as opaque JSON; this page defines the shared interfaces so that any
provider and any subscriber of a topic work together. See
[extensions](extensions.md#broadcasts-between-plugins-preview) for the host rules.

A topic name is `<dot.separated.path>/<version>`. A version changes only for incompatible
payloads; providers may offer several versions side by side. Within a version, consumers
must ignore fields they do not know, and providers only add optional fields.

## `serein.captions/1`

Live captions of speech in a call, for example from a captioning plugin to a recorder that
writes subtitle tracks.

| Field | Type | Meaning |
| --- | --- | --- |
| `call` | string | Channel ID of the call. |
| `user` | string | User ID of the speaker. |
| `stream` | string | The audio stream the text belongs to, `audio:<user ID>`. |
| `start_us`, `end_us` | integer | Span of the speech in microseconds on the host media clock that the recorder's packet tap also uses, so subtitles align with recorded audio. |
| `text` | string | Caption in `source_lang`, at most 2,000 characters. |
| `source_lang` | string | BCP 47 language tag of the speech, such as `de`, when known. |
| `translations` | array | Optional `{"lang": "<BCP 47>", "text": "..."}` entries. |
| `final` | boolean | `false` for a partial caption that a later one with the same `call`, `stream` and `start_us` replaces. |
| `revision` | integer | Increases with each replacement of the same caption; consumers keep the highest. |

```json
{
  "call": "1300000000000000000", "user": "1200000000000000000",
  "stream": "audio:1200000000000000000",
  "start_us": 81234000, "end_us": 83900000,
  "text": "Hallo zusammen", "source_lang": "de",
  "translations": [{ "lang": "en", "text": "Hi everyone" }],
  "final": true, "revision": 2
}
```

Consumers should expect `gap` deliveries when they fall behind and treat the span they
cover as possibly missing captions. Captions contain speech of call participants; consent
for the providing plugin's audio access covers that data, and a subscriber with `network`
can send it off the device, which the Connections list states.
