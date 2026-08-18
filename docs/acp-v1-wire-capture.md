# Wire capture: ACP v1 protocol shapes

Captured live against the ACP v1 schema per "capture, never recall." Source
of truth for `PermissionOption`, `RequestPermissionOutcome`, `SessionUpdate`,
`ToolCallContent`'s diff shape, and the `initialize` handshake. No later
implementation may hand-write an enum string this document has not pinned.

Captured 2026-08-18 against `agentclientprotocol/agent-client-protocol`
(GitHub org `agentclientprotocol`, formerly under `zed-industries`), default
branch `main`.

## Source identity and staleness anchor

```
$ curl -sL https://api.github.com/repos/agentclientprotocol/agent-client-protocol | python3 -c "import json,sys; d=json.load(sys.stdin); print(d['full_name']); print('default_branch', d['default_branch'])"
agentclientprotocol/agent-client-protocol
default_branch main
```

```
$ curl -sL "https://api.github.com/repos/agentclientprotocol/agent-client-protocol/commits?path=schema/v1/schema.json&per_page=1" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d[0]['sha']); print(d[0]['commit']['author']['date'])"
ccff4e7d2e431880225804a8c136c2ccfcb313d0
2026-07-27T10:38:23Z
```

Package-level version pin, from the schema crate's own changelog:

```
$ curl -sL "https://raw.githubusercontent.com/agentclientprotocol/agent-client-protocol/main/schema/v1/CHANGELOG.md" | head -12
# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.20.0](https://github.com/agentclientprotocol/agent-client-protocol/compare/schema-v1.19.1...schema-v1.20.0) - 2026-07-21

### Added

- *(unstable)* add tool call name ([#1752](https://github.com/agentclientprotocol/agent-client-protocol/pull/1752))
```

**Re-verify staleness of this capture** by re-running the two commands
above: if the commit SHA for `schema/v1/schema.json` differs from
`ccff4e7d2e431880225804a8c136c2ccfcb313d0`, or the top changelog entry is
newer than `1.20.0` / `2026-07-21`, this document may have drifted and must
be re-captured before being cited.

The `schema.json` file fetched below:

```
$ curl -sL "https://raw.githubusercontent.com/agentclientprotocol/agent-client-protocol/main/schema/v1/schema.json" -o schema-v1.json
$ wc -c schema-v1.json
242013 schema-v1.json
```

`meta.json` (same commit) confirms the wire protocol version and the full
method-name table:

```
$ curl -sL "https://raw.githubusercontent.com/agentclientprotocol/agent-client-protocol/main/schema/v1/meta.json"
{
  "version": 1,
  "agentMethods": {
    "initialize": "initialize",
    "authenticate": "authenticate",
    "session_new": "session/new",
    "session_load": "session/load",
    "session_set_mode": "session/set_mode",
    "session_set_config_option": "session/set_config_option",
    "session_prompt": "session/prompt",
    "session_cancel": "session/cancel",
    "session_list": "session/list",
    "session_delete": "session/delete",
    "session_resume": "session/resume",
    "session_close": "session/close",
    "logout": "logout"
  },
  "clientMethods": {
    "session_request_permission": "session/request_permission",
    "session_update": "session/update",
    "fs_write_text_file": "fs/write_text_file",
    "fs_read_text_file": "fs/read_text_file",
    "terminal_create": "terminal/create",
    "terminal_output": "terminal/output",
    "terminal_release": "terminal/release",
    "terminal_wait_for_exit": "terminal/wait_for_exit",
    "terminal_kill": "terminal/kill",
    "elicitation_create": "elicitation/create",
    "elicitation_complete": "elicitation/complete"
  },
  "protocolMethods": {
    "cancel_request": "$/cancel_request"
  }
}
```

`meta.json`'s top-level `"version": 1` is the protocol's own self-report,
independent of the `schema-v1.20.0` package/crate version above; the two
are pinned separately because a package bump can land without a wire
protocol version bump (non-breaking changes ship via capabilities, not a
`protocolVersion` bump; see the `initialize` section below).

## `protocolVersion` and the `initialize` handshake

`ProtocolVersion`'s `$defs` entry:

```
$ python3 -c "
import json
d = json.load(open('schema-v1.json'))
print(json.dumps(d['\$defs']['ProtocolVersion'], indent=2))
"
{
  "description": "Protocol version identifier.\n\nThis version is only bumped for breaking changes.\nNon-breaking changes should be introduced via capabilities.",
  "type": "integer",
  "format": "uint16",
  "minimum": 0,
  "maximum": 65535
}
```

Worked `initialize` request/response pair, verbatim from
`docs/protocol/v1/initialization.mdx`:

```
$ curl -sL "https://raw.githubusercontent.com/agentclientprotocol/agent-client-protocol/main/docs/protocol/v1/initialization.mdx" | sed -n '31,82p'
```

Request:

```json
{
  "jsonrpc": "2.0",
  "id": 0,
  "method": "initialize",
  "params": {
    "protocolVersion": 1,
    "clientCapabilities": {
      "fs": {
        "readTextFile": true,
        "writeTextFile": true
      },
      "terminal": true
    },
    "clientInfo": {
      "name": "my-client",
      "title": "My Client",
      "version": "1.0.0"
    }
  }
}
```

Response:

```json
{
  "jsonrpc": "2.0",
  "id": 0,
  "result": {
    "protocolVersion": 1,
    "agentCapabilities": {
      "loadSession": true,
      "promptCapabilities": {
        "image": true,
        "audio": true,
        "embeddedContext": true
      },
      "mcpCapabilities": {
        "http": true,
        "sse": true
      }
    },
    "agentInfo": {
      "name": "my-agent",
      "title": "My Agent",
      "version": "1.0.0"
    },
    "authMethods": []
  }
}
```

`protocolVersion` is a bare integer, not a string: the wire value pinned for
view's `initialize` call is the JSON integer `1`.

## `PermissionOption`

```
$ python3 -c "
import json
d = json.load(open('schema-v1.json'))
print(json.dumps(d['\$defs']['PermissionOption'], indent=2))
print(json.dumps(d['\$defs']['PermissionOptionKind'], indent=2))
"
```

Raw output (the true, unedited result of the command above):

```json
{
  "description": "An option presented to the user when requesting permission.",
  "type": "object",
  "properties": {
    "optionId": {
      "description": "Unique identifier for this permission option.",
      "allOf": [
        {
          "$ref": "#/$defs/PermissionOptionId"
        }
      ]
    },
    "name": {
      "description": "Human-readable label to display to the user.",
      "type": "string"
    },
    "kind": {
      "description": "Hint about the nature of this permission option.",
      "allOf": [
        {
          "$ref": "#/$defs/PermissionOptionKind"
        }
      ]
    },
    "_meta": {
      "description": "The _meta property is reserved by ACP to allow clients and agents to attach additional\nmetadata to their interactions. Implementations MUST NOT make assumptions about values at\nthese keys.\n\nSee protocol docs: [Extensibility](https://agentclientprotocol.com/protocol/extensibility)",
      "type": [
        "object",
        "null"
      ],
      "x-deserialize-default-on-error": true,
      "additionalProperties": true
    }
  },
  "required": [
    "optionId",
    "name",
    "kind"
  ]
}
```

`PermissionOptionKind`'s enum strings (`oneOf` of string `const`s), verbatim,
all four:

Raw output:

```json
{
  "description": "The type of permission option being presented to the user.\n\nHelps clients choose appropriate icons and UI treatment.",
  "oneOf": [
    {
      "description": "Allow this operation only this time.",
      "type": "string",
      "const": "allow_once"
    },
    {
      "description": "Allow this operation and remember the choice.",
      "type": "string",
      "const": "allow_always"
    },
    {
      "description": "Reject this operation only this time.",
      "type": "string",
      "const": "reject_once"
    },
    {
      "description": "Reject this operation and remember the choice.",
      "type": "string",
      "const": "reject_always"
    }
  ]
}
```

Pinned `PermissionOptionKind` wire strings: `"allow_once"`, `"allow_always"`,
`"reject_once"`, `"reject_always"`. These are `snake_case`, not
`kebab-case` or `camelCase`; the research pass's uncertainty is resolved.

## `RequestPermissionOutcome`

```
$ python3 -c "
import json
d = json.load(open('schema-v1.json'))
print(json.dumps(d['\$defs']['RequestPermissionOutcome'], indent=2))
print(json.dumps(d['\$defs']['SelectedPermissionOutcome'], indent=2))
"
```

Raw output:

```json
{
  "description": "The outcome of a permission request.",
  "oneOf": [
    {
      "description": "The prompt turn was cancelled before the user responded.\n\nWhen a client sends a `session/cancel` notification to cancel an ongoing\nprompt turn, it MUST respond to all pending `session/request_permission`\nrequests with this `Cancelled` outcome.\n\nSee protocol docs: [Cancellation](https://agentclientprotocol.com/protocol/prompt-turn#cancellation)",
      "type": "object",
      "properties": {
        "outcome": {
          "type": "string",
          "const": "cancelled"
        }
      },
      "required": [
        "outcome"
      ]
    },
    {
      "description": "The user selected one of the provided options.",
      "type": "object",
      "properties": {
        "outcome": {
          "type": "string",
          "const": "selected"
        }
      },
      "required": [
        "outcome"
      ],
      "allOf": [
        {
          "$ref": "#/$defs/SelectedPermissionOutcome"
        }
      ]
    }
  ],
  "discriminator": {
    "propertyName": "outcome"
  }
}
```

`SelectedPermissionOutcome`'s raw output (same command form, second
`$defs` key):

```json
{
  "description": "The user selected one of the provided options.",
  "type": "object",
  "properties": {
    "optionId": {
      "description": "The ID of the option the user selected.",
      "allOf": [
        {
          "$ref": "#/$defs/PermissionOptionId"
        }
      ]
    },
    "_meta": {
      "description": "The _meta property is reserved by ACP to allow clients and agents to attach additional\nmetadata to their interactions. Implementations MUST NOT make assumptions about values at\nthese keys.\n\nSee protocol docs: [Extensibility](https://agentclientprotocol.com/protocol/extensibility)",
      "type": [
        "object",
        "null"
      ],
      "x-deserialize-default-on-error": true,
      "additionalProperties": true
    }
  },
  "required": [
    "optionId"
  ]
}
```

`SelectedPermissionOutcome` (merged into the `"selected"` variant via
`allOf`): one required field, `optionId` (`PermissionOptionId`, a string).

Pinned `RequestPermissionOutcome` variant shapes:
- `{"outcome": "cancelled"}`, no other fields.
- `{"outcome": "selected", "optionId": "<PermissionOptionId>"}`.

Two variants total, discriminated on the `outcome` string field. The
`Cancelled` variant's own schema description states the normative trigger
verbatim: a client-received `session/cancel` "MUST respond to all pending
`session/request_permission` requests with this `Cancelled` outcome."

Worked example, `docs/protocol/v1/tool-calls.mdx` (`## Requesting
Permission`):

```
$ curl -sL "https://raw.githubusercontent.com/agentclientprotocol/agent-client-protocol/main/docs/protocol/v1/tool-calls.mdx" | sed -n '112,180p'
```

```json
{
  "jsonrpc": "2.0",
  "id": 5,
  "method": "session/request_permission",
  "params": {
    "sessionId": "sess_abc123def456",
    "toolCall": {
      "toolCallId": "call_001"
    },
    "options": [
      {
        "optionId": "allow-once",
        "name": "Allow once",
        "kind": "allow_once"
      },
      {
        "optionId": "reject-once",
        "name": "Reject",
        "kind": "reject_once"
      }
    ]
  }
}
```

```json
{
  "jsonrpc": "2.0",
  "id": 5,
  "result": {
    "outcome": {
      "outcome": "selected",
      "optionId": "allow-once"
    }
  }
}
```

```json
{
  "jsonrpc": "2.0",
  "id": 5,
  "result": {
    "outcome": {
      "outcome": "cancelled"
    }
  }
}
```

## `SessionUpdate` discriminant list: SURPRISE, 11 variants, not 5

```
$ python3 -c "
import json
d = json.load(open('schema-v1.json'))
su = d['\$defs']['SessionUpdate']
for v in su['oneOf']:
    print(v['properties']['sessionUpdate']['const'])
"
user_message_chunk
agent_message_chunk
agent_thought_chunk
tool_call
tool_call_update
plan
available_commands_update
current_mode_update
config_option_update
session_info_update
usage_update
```

**Flag: the schema pins 11 `sessionUpdate` discriminants, six beyond the
five the research pass confirmed (`plan`, `agent_message_chunk`,
`tool_call`, `tool_call_update`, `usage_update`).** The six additional
discriminants, each with its own required companion payload merged via
`allOf`:

| `sessionUpdate` const | payload `$ref` | description (verbatim) |
|---|---|---|
| `user_message_chunk` | `ContentChunk` | "A chunk of the user's message being streamed." |
| `agent_thought_chunk` | `ContentChunk` | "A chunk of the agent's internal reasoning being streamed." |
| `available_commands_update` | `AvailableCommandsUpdate` | "Available commands are ready or have changed" |
| `current_mode_update` | `CurrentModeUpdate` | "The current mode of the session has changed" |
| `config_option_update` | `ConfigOptionUpdate` | "Session configuration options have been updated." |
| `session_info_update` | `SessionInfoUpdate` | "Session metadata has been updated (title, timestamps, custom metadata)" |

All 11 variants discriminate on the `sessionUpdate` string property
(`"discriminator": {"propertyName": "sessionUpdate"}`). Later tasks that
build the `Msg` enum for `session/update` handling own six more arms than
planned: `UserMessageChunk`, `AgentThoughtChunk`,
`AvailableCommandsUpdate`, `CurrentModeUpdate`, `ConfigOptionUpdate`,
`SessionInfoUpdate`, in addition to the five already planned for.

## `ToolCallContent`'s `"diff"` variant

```
$ python3 -c "
import json
d = json.load(open('schema-v1.json'))
print(json.dumps(d['\$defs']['ToolCallContent'], indent=2))
print(json.dumps(d['\$defs']['Diff'], indent=2))
"
```

`ToolCallContent` is a three-way `oneOf` discriminated on `type`:
`"content"` (merges `Content`), `"diff"` (merges `Diff`), `"terminal"`
(merges `Terminal`). Its raw `$defs` output (`ToolCallContent`, then
`Diff`, the two keys the command above prints):

```json
{
  "description": "Content produced by a tool call.\n\nTool calls can produce different types of content including\nstandard content blocks (text, images) or file diffs.\n\nSee protocol docs: [Content](https://agentclientprotocol.com/protocol/tool-calls#content)",
  "oneOf": [
    {
      "description": "Standard content block (text, images, resources).",
      "type": "object",
      "properties": {
        "type": {
          "type": "string",
          "const": "content"
        }
      },
      "required": [
        "type"
      ],
      "allOf": [
        {
          "$ref": "#/$defs/Content"
        }
      ]
    },
    {
      "description": "File modification shown as a diff.",
      "type": "object",
      "properties": {
        "type": {
          "type": "string",
          "const": "diff"
        }
      },
      "required": [
        "type"
      ],
      "allOf": [
        {
          "$ref": "#/$defs/Diff"
        }
      ]
    },
    {
      "description": "Embed a terminal created with `terminal/create` by its id.\n\nThe terminal must be added before calling `terminal/release`.\n\nSee protocol docs: [Terminal](https://agentclientprotocol.com/protocol/terminals)",
      "type": "object",
      "properties": {
        "type": {
          "type": "string",
          "const": "terminal"
        }
      },
      "required": [
        "type"
      ],
      "allOf": [
        {
          "$ref": "#/$defs/Terminal"
        }
      ]
    }
  ],
  "discriminator": {
    "propertyName": "type"
  }
}
```

`Diff`'s raw output:

```json
{
  "description": "A diff representing file modifications.\n\nShows changes to files in a format suitable for display in the client UI.\n\nSee protocol docs: [Content](https://agentclientprotocol.com/protocol/tool-calls#content)",
  "type": "object",
  "properties": {
    "path": {
      "description": "The absolute file path being modified.",
      "type": "string"
    },
    "oldText": {
      "description": "The original content (None for new files).",
      "type": [
        "string",
        "null"
      ],
      "x-deserialize-default-on-error": true
    },
    "newText": {
      "description": "The new content after modification.",
      "type": "string"
    },
    "_meta": {
      "description": "The _meta property is reserved by ACP to allow clients and agents to attach additional\nmetadata to their interactions. Implementations MUST NOT make assumptions about values at\nthese keys.\n\nSee protocol docs: [Extensibility](https://agentclientprotocol.com/protocol/extensibility)",
      "type": [
        "object",
        "null"
      ],
      "x-deserialize-default-on-error": true,
      "additionalProperties": true
    }
  },
  "required": [
    "path",
    "newText"
  ]
}
```

Pinned `"diff"` shape: `{"type": "diff", "path": <string>, "oldText":
<string | null>, "newText": <string>}`. `oldText` is nullable (new-file
case) and NOT required; `path` and `newText` are required.

## Permission-overlap reply legitimacy (the pending-permission-request degrade path)

Question: for a second `session/request_permission` arriving while a first
is still unanswered, is a raw JSON-RPC error a conformant reply to
`session/request_permission`, or is only a `RequestPermissionOutcome` value
legal?

`RequestPermissionResponse`'s schema only defines the success (`result`)
shape; it says nothing about error legality one way or the other, because
that is true of every JSON-RPC method's response schema (errors are a
transport-level JSON-RPC concept, never encoded in a method's `result`
schema regardless of method):

```
$ python3 -c "
import json
d = json.load(open('schema-v1.json'))
print(json.dumps(d['\$defs']['RequestPermissionResponse'], indent=2))
"
```

Raw output:

```json
{
  "description": "Response to a permission request.",
  "type": "object",
  "properties": {
    "outcome": {
      "description": "The user's decision on the permission request.",
      "allOf": [
        {
          "$ref": "#/$defs/RequestPermissionOutcome"
        }
      ]
    },
    "_meta": {
      "description": "The _meta property is reserved by ACP to allow clients and agents to attach additional\nmetadata to their interactions. Implementations MUST NOT make assumptions about values at\nthese keys.\n\nSee protocol docs: [Extensibility](https://agentclientprotocol.com/protocol/extensibility)",
      "type": [
        "object",
        "null"
      ],
      "x-deserialize-default-on-error": true,
      "additionalProperties": true
    }
  },
  "required": [
    "outcome"
  ],
  "x-side": "client",
  "x-method": "session/request_permission"
}
```

Two prose sources bear on the question, and a careful read of both against
the *same* triggering scenario finds that they contradict each other,
not that they cleanly divide the space by who initiated cancellation.

**Source 1, `docs/protocol/v1/prompt-turn.mdx`, its Cancellation section:**

```
$ curl -sL "https://raw.githubusercontent.com/agentclientprotocol/agent-client-protocol/main/docs/protocol/v1/prompt-turn.mdx" | sed -n '312,332p'
```

> "The Client **MUST** respond to all pending `session/request_permission`
> requests with the `cancelled` outcome."

This is a **`RequestPermissionOutcome` value** (`{"outcome": {"outcome":
"cancelled"}}`), a valid JSON-RPC *result*, not an error. The trigger this
prose names is the client sending `session/cancel` to cancel the whole
prompt turn.

**Source 2, `docs/protocol/v1/cancellation.mdx`, its Cascading
Cancellation Flow worked example:**

```
$ curl -sL "https://raw.githubusercontent.com/agentclientprotocol/agent-client-protocol/main/docs/protocol/v1/cancellation.mdx" | sed -n '10,68p'
```

> "**MUST** send one of these responses for the original request: A valid
> response with appropriate data ... OR An error response with code
> `-32800` (Request Cancelled)"

and the cascading example's mermaid diagram, numbered steps verbatim:

> `Note over Client,Agent: 3. Client cancels the prompt turn`
> `Client->>Agent: session/cancel (sessionId)`
>
> `Note over Client,Agent: 4. Agent cascades cancellation internally`
> `Agent->>Client: $/cancel_request (id=3) [permission request]`
>
> `Note over Client,Agent: 5. Client confirms individual cancellations`
> `Client->>Agent: response to id=3 (error -32800 "Cancelled")`

**These two sources are describing the identical trigger and the identical
pending-request type, and they prescribe two different response bodies for
it.** The cascading example's own annotation "3. Client cancels the prompt
turn" is a client-initiated `session/cancel` of the whole prompt turn, the
exact same trigger Source 1's MUST governs, not an independent
agent-initiated event. The following annotation, "4. Agent cascades
cancellation internally," shows `$/cancel_request` as downstream of, and
part of the same flow as, that `session/cancel`; the example does not
depict the agent cancelling on its own initiative (that is
`cancellation.mdx`'s separate "Internal Cancellation" section, e.g. "LLM
context limit reached", which genuinely is independent of any client
trigger and is not what this worked example shows). Yet for that one
client-initiated-whole-turn-cancel case, applied to the identical kind of
pending request (`session/request_permission`, labeled "[permission
request]" in the diagram): `prompt-turn.mdx` mandates a
`RequestPermissionOutcome` `"cancelled"` result, while `cancellation.mdx`'s
own worked example for the same trigger shows a raw JSON-RPC `-32800` error
instead.

**This is a genuine contradiction in the upstream ACP v1 docs, not two
non-competing rules keyed on who initiated cancellation.** Both quotes are
byte-exact above; no interpretation reconciles them for the
client-cancels-the-whole-turn case. What this does establish without
ambiguity: a raw JSON-RPC error is shown, in the spec's own worked example,
as a body the agent must be prepared to receive in place of a
`RequestPermissionOutcome` for a pending `session/request_permission`
(matching the brief's option (a), at minimum for this triggering path).
What it does not establish: the spec does not consistently say
error-vs-outcome is determined by who initiated cancellation; its own two
pages disagree with each other on that same case.

Neither source directly discusses the exact overlap scenario the degrade
path handles ("a second `session/request_permission` arrives while a first
is unanswered," with no cancellation in play at all): that is not a
cancellation scenario in either source's terms, and no ACP doc page or
schema field addresses concurrent/overlapping permission requests
specifically. The overlap degrade path is therefore a view-side policy
choice, not one dictated by the wire spec, and it inherits an upstream spec
that disagrees with itself on the closest analogous case (whole-turn
cancellation) rather than a clean, resolvable rule. Deciding how the
degrade path should behave given that inconsistency is a downstream design
call, not a fact this capture pins.

**Reference/example agent implementation:** not discoverable. The
`agentclientprotocol/agent-client-protocol` repository ships only the
schema crate, the schema generator, and the docs site; no reference or
example agent implementation exists in this repo to cross-check error
handling against
(`agent-client-protocol-schema/`, `schema-generator/`, `schema/`, `docs/`,
`scripts/` are the only source directories at the repo root).
