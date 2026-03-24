# Socket API Reference

Forgetty exposes a JSON-RPC 2.0 API over a Unix domain socket (named pipe on
Windows). This allows external tools, scripts, and AI agents to control the
terminal programmatically.

## Connection

By default, the socket is created at:

| Platform | Path |
|----------|------|
| Linux    | `$XDG_RUNTIME_DIR/forgetty.sock` |
| macOS    | `$TMPDIR/forgetty.sock` |
| Windows  | `\\.\pipe\forgetty` |

The path can be overridden via the `socket.path` config key or the
`FORGETTY_SOCKET` environment variable.

## Protocol

All messages follow the [JSON-RPC 2.0](https://www.jsonrpc.org/specification)
format. Requests and responses are newline-delimited JSON.

### Example request

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "tab.create",
  "params": { "cwd": "/home/user/project" }
}
```

### Example response

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": { "tab_id": "a1b2c3d4" }
}
```

## Methods

### Tab Management

#### `tab.list`

List all open tabs.

**Params:** none

**Result:**
```json
{
  "tabs": [
    {
      "id": "a1b2c3d4",
      "title": "zsh",
      "cwd": "/home/user/project",
      "git_branch": "main",
      "is_focused": true
    }
  ]
}
```

#### `tab.create`

Create a new tab.

**Params:**
| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `cwd` | string | no | Working directory for the new tab |
| `command` | string | no | Command to run instead of the default shell |
| `workspace` | string | no | Workspace to create the tab in |

**Result:** `{ "tab_id": "..." }`

#### `tab.close`

Close a tab.

**Params:**
| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `tab_id` | string | yes | ID of the tab to close |

**Result:** `{}`

#### `tab.focus`

Focus a tab.

**Params:**
| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `tab_id` | string | yes | ID of the tab to focus |

**Result:** `{}`

### Pane Management

#### `pane.list`

List all panes in a tab.

**Params:**
| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `tab_id` | string | yes | ID of the tab |

**Result:**
```json
{
  "panes": [
    {
      "id": "p1",
      "rows": 24,
      "cols": 80,
      "cwd": "/home/user/project",
      "is_focused": true
    }
  ]
}
```

#### `pane.split`

Split the focused pane.

**Params:**
| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `direction` | string | yes | `"vertical"` or `"horizontal"` |
| `pane_id` | string | no | Pane to split (default: focused pane) |

**Result:** `{ "pane_id": "..." }`

#### `pane.close`

Close a pane.

**Params:**
| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `pane_id` | string | yes | ID of the pane to close |

**Result:** `{}`

### Terminal I/O

#### `terminal.write`

Send input to a pane's PTY (as if typed by the user).

**Params:**
| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `pane_id` | string | yes | Target pane |
| `data` | string | yes | Text to write to the PTY |

**Result:** `{}`

#### `terminal.read`

Read recent output from a pane.

**Params:**
| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `pane_id` | string | yes | Target pane |
| `lines` | number | no | Number of lines to read (default: 100) |

**Result:**
```json
{
  "output": "$ cargo build\n   Compiling forgetty v0.1.0\n    Finished ...\n"
}
```

#### `terminal.resize`

Resize a pane.

**Params:**
| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `pane_id` | string | yes | Target pane |
| `rows` | number | yes | New row count |
| `cols` | number | yes | New column count |

**Result:** `{}`

### Workspace Management

#### `workspace.list`

List all workspaces.

**Params:** none

**Result:**
```json
{
  "workspaces": [
    {
      "id": "w1",
      "name": "forgetty",
      "tab_count": 3,
      "is_active": true
    }
  ]
}
```

#### `workspace.create`

Create a new workspace.

**Params:**
| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | string | yes | Workspace name |

**Result:** `{ "workspace_id": "..." }`

#### `workspace.switch`

Switch to a workspace.

**Params:**
| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `workspace_id` | string | yes | ID of the workspace |

**Result:** `{}`

#### `workspace.save`

Save the current workspace layout to disk.

**Params:** none

**Result:** `{}`

### Notifications

#### `notification.list`

List panes with pending notifications.

**Params:** none

**Result:**
```json
{
  "notifications": [
    {
      "pane_id": "p1",
      "tab_id": "a1b2c3d4",
      "matched_pattern": "Error:",
      "line": "error[E0308]: mismatched types",
      "timestamp": "2026-01-15T10:30:00Z"
    }
  ]
}
```

#### `notification.dismiss`

Dismiss notifications for a pane.

**Params:**
| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `pane_id` | string | yes | Pane to dismiss notifications for |

**Result:** `{}`

### System

#### `version`

Get the Forgetty version.

**Params:** none

**Result:** `{ "version": "0.1.0" }`

#### `config.reload`

Reload the configuration file.

**Params:** none

**Result:** `{}`

## Error Codes

| Code | Message | Description |
|------|---------|-------------|
| -32600 | Invalid Request | Malformed JSON-RPC |
| -32601 | Method not found | Unknown method name |
| -32602 | Invalid params | Missing or invalid parameters |
| -32603 | Internal error | Unexpected server error |
| 1001 | Tab not found | The specified tab ID does not exist |
| 1002 | Pane not found | The specified pane ID does not exist |
| 1003 | Workspace not found | The specified workspace ID does not exist |

## CLI Usage

You can interact with the socket using standard tools:

```sh
# Using socat
echo '{"jsonrpc":"2.0","id":1,"method":"tab.list"}' | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/forgetty.sock

# Using ncat
echo '{"jsonrpc":"2.0","id":1,"method":"version"}' | ncat -U $XDG_RUNTIME_DIR/forgetty.sock
```
