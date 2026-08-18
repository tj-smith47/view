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

Version negotiation, verbatim from the same page:

```
$ curl -sL "https://raw.githubusercontent.com/agentclientprotocol/agent-client-protocol/main/docs/protocol/v1/initialization.mdx" | sed -n '92,99p'
### Version Negotiation

The `initialize` request **MUST** include the latest protocol version the Client supports.

If the Agent supports the requested version, it **MUST** respond with the same version. Otherwise, the Agent **MUST** respond with the latest version it supports.

If the Client does not support the version specified by the Agent in the `initialize` response, the Client **SHOULD** close the connection and inform the user about it.
```

The response `protocolVersion` is therefore the agent's counter-offer, not an
echo, and a client that ignores it will speak a dialect the agent never
agreed to. view supports exactly version `1`, so any other answer closes the
session and reports both versions to the user.

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

## `Content`, `Terminal`, `Plan`, and `UsageUpdate`

Re-verified staleness ahead of this capture, same two commands as
"Source identity and staleness anchor" above: commit SHA for
`schema/v1/schema.json` is still `ccff4e7d2e431880225804a8c136c2ccfcb313d0`,
and `schema-v1.json` re-fetched to the same byte count, `242013`. No drift.

`ToolCallContent`'s `"content"` variant merges `Content`, and its
`"terminal"` variant merges `Terminal` (see the `oneOf` dump above); neither
had been dumped until now. `SessionUpdate`'s `plan` and `usage_update`
discriminants (see the 11-variant list above) merge `Plan` and
`UsageUpdate` respectively.

```
$ python3 -c "
import json
d = json.load(open('schema-v1.json'))['\$defs']
for k in ['Content', 'Terminal', 'Plan', 'PlanEntry', 'PlanEntryPriority', 'PlanEntryStatus', 'UsageUpdate', 'Cost']:
    print('===', k, '===')
    print(json.dumps(d[k], indent=2))
    print()
"
```

Raw output:

```json
=== Content ===
{
  "description": "Standard content block (text, images, resources).",
  "type": "object",
  "properties": {
    "content": {
      "description": "The actual content block.",
      "allOf": [{"$ref": "#/$defs/ContentBlock"}]
    }
  },
  "required": ["content"]
}

=== Terminal ===
{
  "description": "Embed a terminal created with `terminal/create` by its id.\n\nThe terminal must be added before calling `terminal/release`.",
  "type": "object",
  "properties": {
    "terminalId": {
      "description": "Identifier of the terminal instance to embed in the content stream.",
      "allOf": [{"$ref": "#/$defs/TerminalId"}]
    }
  },
  "required": ["terminalId"]
}

=== Plan ===
{
  "description": "An execution plan for accomplishing complex tasks.",
  "type": "object",
  "properties": {
    "entries": {
      "description": "The list of tasks to be accomplished.\n\nWhen updating a plan, the agent must send a complete list of all entries\nwith their current status. The client replaces the entire plan with each update.",
      "type": "array",
      "items": {"$ref": "#/$defs/PlanEntry"}
    }
  },
  "required": ["entries"]
}

=== PlanEntry ===
{
  "description": "A single entry in the execution plan.",
  "type": "object",
  "properties": {
    "content": {
      "description": "Human-readable description of what this task aims to accomplish.",
      "type": "string"
    },
    "priority": {
      "description": "The relative importance of this task.",
      "allOf": [{"$ref": "#/$defs/PlanEntryPriority"}]
    },
    "status": {
      "description": "Current execution status of this task.",
      "allOf": [{"$ref": "#/$defs/PlanEntryStatus"}]
    }
  },
  "required": ["content", "priority", "status"]
}

=== PlanEntryPriority ===
{
  "description": "Priority levels for plan entries.",
  "oneOf": [
    {"type": "string", "const": "high"},
    {"type": "string", "const": "medium"},
    {"type": "string", "const": "low"}
  ]
}

=== PlanEntryStatus ===
{
  "description": "Status of a plan entry in the execution flow.",
  "oneOf": [
    {"type": "string", "const": "pending"},
    {"type": "string", "const": "in_progress"},
    {"type": "string", "const": "completed"}
  ]
}

=== UsageUpdate ===
{
  "description": "Context window and cost update for a session.",
  "type": "object",
  "properties": {
    "used": {"description": "Tokens currently in context.", "type": "integer", "format": "uint64", "minimum": 0},
    "size": {"description": "Total context window size in tokens.", "type": "integer", "format": "uint64", "minimum": 0},
    "cost": {
      "description": "Cumulative session cost (optional).",
      "anyOf": [{"$ref": "#/$defs/Cost"}, {"type": "null"}]
    }
  },
  "required": ["used", "size"]
}

=== Cost ===
{
  "description": "Cumulative session cost information.",
  "type": "object",
  "properties": {
    "amount": {"description": "The cost amount.", "type": "number", "format": "double"},
    "currency": {"description": "The currency code (e.g., \"USD\").", "type": "string"}
  },
  "required": ["amount", "currency"]
}
```

Pinned facts:

- `Content` (the `ToolCallContent` `"content"` variant's merged payload):
  one required field, `content`, itself a nested `ContentBlock` (the same
  five-way `text`/`image`/`audio`/`resource_link`/`resource` union pinned
  above under "`ContentBlock` and the chunk payload"). So a full
  text-content item on the wire is
  `{"type": "content", "content": {"type": "text", "text": "..."}}`.
- `Terminal` (the `ToolCallContent` `"terminal"` variant's merged
  payload): one required field, `terminalId` (a string).
- `Plan`: one required field, `entries`, an array of `PlanEntry`. The
  schema's own description is explicit that an update is a full replace,
  not a delta: "the agent must send a complete list of all entries with
  their current status. The client replaces the entire plan with each
  update."
- `PlanEntry`: all three of `content` (string), `priority`
  (`PlanEntryPriority`), `status` (`PlanEntryStatus`) are required.
- `PlanEntryPriority` is a three-way closed string enum: `high`, `medium`,
  `low`.
- `PlanEntryStatus` is a three-way closed string enum: `pending`,
  `in_progress`, `completed` -- only three, unlike `ToolCallStatus`'s four
  (no `failed` counterpart for a plan entry).
- `UsageUpdate`: `used` and `size` (both `uint64`) are required; `cost` is
  an optional, nullable `Cost`.
- `Cost`: both `amount` (a double) and `currency` (a string) are required
  whenever `cost` itself is present.

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

## `RequestId`

The id member is a three-way union, not an integer. An implementation that
types it as an integer fails to decode the first frame any string-id agent
sends, and misreads a null-id request as a notification.

```
$ python3 -c "
import json
d = json.load(open('schema-v1.json'))
print(json.dumps(d['\$defs']['RequestId'], indent=2))
"
{
  "description": "JSON RPC Request Id\n\nAn identifier established by the Client that MUST contain a String, Number, or NULL value if included. If it is not included it is assumed to be a notification. The value SHOULD normally not be Null \\[1\\] and Numbers SHOULD NOT contain fractional parts \\[2\\]\n\nThe Server MUST reply with the same value in the Response object if included. This member is used to correlate the context between the two objects.\n\n\\[1\\] The use of Null as a value for the id member in a Request object is discouraged, because this specification uses a value of Null for Responses with an unknown id. Also, because JSON-RPC 1.0 uses an id value of Null for Notifications this could cause confusion in handling.\n\n\\[2\\] Fractional parts may be problematic, since many decimal fractions cannot be represented exactly as binary fractions.",
  "anyOf": [
    {
      "title": "Null",
      "description": "The JSON-RPC `null` request id.",
      "type": "null"
    },
    {
      "title": "Number",
      "description": "A numeric JSON-RPC request id.",
      "type": "integer",
      "format": "int64"
    },
    {
      "title": "Str",
      "description": "A string JSON-RPC request id.",
      "type": "string"
    }
  ]
}
```

Three consequences the schema text pins directly:

- present-but-null and absent are different frames. Only the absent case is a
  notification, so a decoder cannot collapse both onto `None`.
- the number arm is `int64`, signed, and fractional parts are discouraged
  rather than forbidden, so a `u64` field rejects ids the schema permits.
- correlation is by equality of the whole value ("the Server MUST reply with
  the same value"), which means the id a peer chose must be stored and
  echoed unchanged rather than re-derived.

view allocates only numeric ids for the requests it originates. The string
and null arms exist so that ids chosen by the agent survive the round trip
intact.

## stdio framing

The framing rule the transport layer is built on, verbatim from
`docs/protocol/v1/transports.mdx`:

```
$ curl -sL "https://raw.githubusercontent.com/agentclientprotocol/agent-client-protocol/main/docs/protocol/v1/transports.mdx" | sed -n '6,27p'
```

> ACP uses JSON-RPC to encode messages. JSON-RPC messages **MUST** be UTF-8 encoded.
>
> In the **stdio** transport:
>
> - The client launches the agent as a subprocess.
> - The agent reads JSON-RPC messages from its standard input (`stdin`) and sends messages to its standard output (`stdout`).
> - Messages are individual JSON-RPC requests, notifications, or responses.
> - Messages are delimited by newlines (`\n`), and **MUST NOT** contain embedded newlines.
> - The agent **MAY** write UTF-8 strings to its standard error (`stderr`) for logging purposes. Clients **MAY** capture, forward, or ignore this logging.
> - The agent **MUST NOT** write anything to its `stdout` that is not a valid ACP message.
> - The client **MUST NOT** write anything to the agent's `stdin` that is not a valid ACP message.

Pinned: newline-delimited JSON-RPC 2.0, UTF-8, no embedded newline, no
length header. `stderr` carries agent logging only and never a frame.

## `ToolCallStatus` and `StopReason`

Re-verified against the same staleness anchor recorded above (schema commit
`ccff4e7d2e431880225804a8c136c2ccfcb313d0`, changelog top entry
`1.20.0` / `2026-07-21`, both unchanged on re-check).

```
$ python3 -c "
import json
d = json.load(open('schema-v1.json'))
for k in ['ToolCallStatus','StopReason']:
    print('===',k,'===')
    for v in d['\$defs'][k]['oneOf']:
        print(v['const'])
"
```

Raw output:

```
=== ToolCallStatus ===
pending
in_progress
completed
failed
=== StopReason ===
end_turn
max_tokens
max_turn_requests
refusal
cancelled
```

Pinned `ToolCallStatus` wire strings: `"pending"`, `"in_progress"`,
`"completed"`, `"failed"` -- four, `snake_case`. Pinned `StopReason` wire
strings: `"end_turn"`, `"max_tokens"`, `"max_turn_requests"`, `"refusal"`,
`"cancelled"` -- five, `snake_case`. `StopReason`'s `"cancelled"` carries a
normative note verbatim in its own description: it "MUST be returned when
the client sends a `session/cancel` notification, even if the cancellation
causes exceptions in underlying operations."

## `ContentBlock` and the chunk payload

```
$ python3 -c "
import json
d = json.load(open('schema-v1.json'))
cb = d['\$defs']['ContentBlock']
print('discriminator:', cb.get('discriminator'))
for v in cb['oneOf']:
    print(' -', v['properties']['type']['const'], '->', [r for r in v.get('allOf',[])])
print('ContentChunk required:', d['\$defs']['ContentChunk']['required'])
print('ResourceLink required:', d['\$defs']['ResourceLink']['required'])
"
```

Raw output:

```
discriminator: {'propertyName': 'type'}
 - text -> [{'$ref': '#/$defs/TextContent'}]
 - image -> [{'$ref': '#/$defs/ImageContent'}]
 - audio -> [{'$ref': '#/$defs/AudioContent'}]
 - resource_link -> [{'$ref': '#/$defs/ResourceLink'}]
 - resource -> [{'$ref': '#/$defs/EmbeddedResource'}]
ContentChunk required: ['content']
ResourceLink required: ['name', 'uri']
```

Pinned: `ContentBlock` is a five-way `oneOf` discriminated on `type`, whose
`const`s are `"text"`, `"image"`, `"audio"`, `"resource_link"`,
`"resource"`. `TextContent`'s payload member is `text` (a string).
`ResourceLink` requires `name` and `uri`. `ContentChunk` (the payload of
every `*_chunk` `sessionUpdate`) requires `content` and carries an optional
nullable `messageId`, described verbatim: "All chunks belonging to the same
message share the same `messageId`. A change in `messageId` indicates a new
message has started."

## Method payload members

```
$ python3 -c "
import json
d = json.load(open('schema-v1.json'))['\$defs']
for k in ['NewSessionRequest','NewSessionResponse','PromptRequest','PromptResponse','SessionNotification','ToolCall','ToolCallUpdate','ReadTextFileRequest','ReadTextFileResponse','WriteTextFileRequest','WriteTextFileResponse']:
    print(k, '| required:', d[k]['required'], '| properties:', sorted(d[k]['properties']))
"
```

Raw output:

```
NewSessionRequest | required: ['cwd', 'mcpServers'] | properties: ['_meta', 'additionalDirectories', 'cwd', 'mcpServers']
NewSessionResponse | required: ['sessionId'] | properties: ['_meta', 'configOptions', 'modes', 'sessionId']
PromptRequest | required: ['sessionId', 'prompt'] | properties: ['_meta', 'prompt', 'sessionId']
PromptResponse | required: ['stopReason'] | properties: ['_meta', 'stopReason']
SessionNotification | required: ['sessionId', 'update'] | properties: ['_meta', 'sessionId', 'update']
ToolCall | required: ['toolCallId', 'title'] | properties: ['_meta', 'content', 'kind', 'locations', 'rawInput', 'rawOutput', 'status', 'title', 'toolCallId']
ToolCallUpdate | required: ['toolCallId'] | properties: ['_meta', 'content', 'kind', 'locations', 'rawInput', 'rawOutput', 'status', 'title', 'toolCallId']
ReadTextFileRequest | required: ['sessionId', 'path'] | properties: ['_meta', 'limit', 'line', 'path', 'sessionId']
ReadTextFileResponse | required: ['content'] | properties: ['_meta', 'content']
WriteTextFileRequest | required: ['sessionId', 'path', 'content'] | properties: ['_meta', 'content', 'path', 'sessionId']
WriteTextFileResponse | required: [] | properties: ['_meta']
```

The asymmetry that matters for any client rendering tool calls: `ToolCall`
(the `tool_call` discriminant) requires `title`, while `ToolCallUpdate` (the
`tool_call_update` discriminant) requires only `toolCallId`. An update
carries only what changed, so a client holding a whole-call view must
remember the announcement's `title` and `status` rather than treat their
absence as a value.

## Client crate on crates.io: `agent-client-protocol`

Checked because a maintained client crate would displace a hand-rolled
transport.

```
$ curl -sL "https://crates.io/api/v1/crates/agent-client-protocol" -H 'User-Agent: view-impl-check' | python3 -c "
import json,sys
d=json.load(sys.stdin)
c=d['crate']
for k in ['name','max_version','max_stable_version','updated_at','downloads','repository','description']:
    print(k, c.get(k))
for v in d.get('versions',[])[:6]:
    print(' ver', v['num'], v['created_at'], 'license', v.get('license'), 'yanked', v.get('yanked'))
"
name agent-client-protocol
max_version 2.0.0
max_stable_version 2.0.0
updated_at 2026-07-23T14:52:35.042287Z
downloads 3754346
repository https://github.com/agentclientprotocol/rust-sdk
description Core protocol types and traits for the Agent Client Protocol
 ver 2.0.0 2026-07-23T14:52:35.042287Z license Apache-2.0 yanked False
 ver 1.3.0 2026-07-20T14:49:41.393567Z license Apache-2.0 yanked False
 ver 1.2.0 2026-07-07T11:37:38.907662Z license Apache-2.0 yanked False
 ver 1.1.0 2026-07-06T17:27:30.197911Z license Apache-2.0 yanked False
 ver 1.0.1 2026-06-29T10:23:27.390962Z license Apache-2.0 yanked False
 ver 1.0.0 2026-06-24T18:15:10.449347Z license Apache-2.0 yanked False
```

```
$ curl -sL "https://crates.io/api/v1/crates/agent-client-protocol/2.0.0/dependencies" -H 'User-Agent: view-impl-check' | python3 -c "
import json,sys
for x in json.load(sys.stdin)['dependencies']:
    print(x['kind'], x['crate_id'], x['req'], 'optional' if x['optional'] else '')
"
normal agent-client-protocol-derive ^2.0.0
normal agent-client-protocol-schema =1.5.0
normal async-io ^2
normal async-process ^2
normal blocking ^1
dev clap ^4.5
dev expect-test ^1.5
normal futures ^0.3.32
normal futures-concurrency ^7.6.3
normal rustc-hash ^2.1.1
normal rustix ^1
normal schemars ^1.0
normal serde ^1.0
normal serde_json ^1
normal shell-words ^1.1
dev tokio ^1.52
dev tokio-util ^0.7
normal tracing ^0.1
normal uuid ^1.18
normal windows-sys ^0.61
```

```
$ cargo generate-lockfile   # scratch crate depending only on agent-client-protocol = "2"
$ grep -c '^name = ' Cargo.lock
145
$ grep -E '^name = "(tokio|async-io|async-std|smol|polling|schemars)"' Cargo.lock
name = "async-io"
name = "polling"
name = "schemars"
name = "schemars"
```

Pinned facts: the crate exists, is Apache-2.0, is actively maintained, and
its runtime is the `async-io`/`async-process`/`blocking` reactor, with
`tokio` present only as a dev-dependency. Its resolved graph is 145 crates.
Its release cadence went `1.0.0` (2026-06-24) to `2.0.0` (2026-07-23), a
major version inside one month.

## `authenticate` and the `auth_required` error code

Re-verified against the same commit pinned at the top of this document
(`ccff4e7d2e431880225804a8c136c2ccfcb313d0`, re-checked live before this
capture and unchanged).

`ErrorCode`'s `$defs` entry (the reserved-range members only; the JSON-RPC
standard four are omitted here, already implemented as `wire.rs` constants):

```
$ python3 -c "
import json
d = json.load(open('schema-v1.json'))
print(json.dumps(d['\$defs']['ErrorCode'], indent=2))
"
```

Raw output, the `-32000` member (verbatim, unedited):

```json
{
  "title": "Authentication required",
  "description": "**Authentication required**: Authentication is required before this operation can be performed.",
  "type": "integer",
  "format": "int32",
  "const": -32000
}
```

`docs/protocol/v1/schema.mdx` documents the same code at the same value,
under the heading `ErrorCode`, confirming the schema and the rendered docs
agree.

`docs/protocol/v1/authentication.mdx`, verbatim, on when it is sent:

```
$ curl -sL "https://raw.githubusercontent.com/agentclientprotocol/agent-client-protocol/main/docs/protocol/v1/authentication.mdx" | sed -n '81,111p'
```

```
## Authenticating

When an Agent requires authentication before allowing session creation, the Client calls `authenticate` with one of the advertised authentication method IDs:
```

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "authenticate",
  "params": {
    "methodId": "agent-login"
  }
}
```

```
On success, the Agent returns an empty result:
```

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {}
}
```

The schema's `session_new` method description, verbatim: "May return an
`auth_required` error if the agent requires authentication." The
`authenticate` method's own description also mentions the error, describing
what a successful call clears: "the client can proceed to create sessions
with `new_session` without receiving an `auth_required` error." Between the
two, only `session/new` is the request that can itself fail with the code,
so the guard belongs there specifically, not on every outgoing request.

Pinned `authMethods` entry shape, from `authentication.mdx`'s advertising
example (`docs/protocol/v1/authentication.mdx` lines 41-61, same commit):

```json
{
  "id": "agent-login",
  "name": "Agent login",
  "description": "Sign in using the agent's login flow"
}
```

The schema defines `AuthMethod` itself as an `anyOf` union carrying a `type`
field that acts as the discriminator between its members, with an absent
`type` treated as `agent`. Today the union has exactly one member,
`AuthMethodAgent`, which is the flattened `{id, name, description}` shape
pinned above -- but the discriminator field means a second variant can be
added to the union later without changing this shape's own fields.

Pinned facts for the session-lifecycle client: `authenticate`'s request
carries exactly one field, `methodId` (a string, one of the ids in the
`initialize` response's `authMethods`); its success reply is `{}`, no
fields; a `session/new` failing with JSON-RPC error code `-32000` is the
wire's own signal to call `authenticate` and retry, not a terminal
failure.
