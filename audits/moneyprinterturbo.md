# MoneyPrinterTurbo — Security & Code Audit

**Target:** [`harry0703/MoneyPrinterTurbo`](https://github.com/harry0703/MoneyPrinterTurbo) (commit on `main` at audit time)
**Scope:** `app/`, `webui/`, `Dockerfile`, `docker-compose.yml`, `config.example.toml`, `requirements.txt`
**Out of scope:** the bundled JS/CSS, i18n locales, MoviePy/ffmpeg internals, third-party LLM SDKs

This is a static review of the Python codebase. I read each finding's referenced file and verified the citation. Severity reflects exploitability + impact assuming the documented deployment (Docker on a network reachable by users).

---

## TL;DR

The project is designed as a single-tenant local tool but ships with deployment artifacts (Dockerfile, docker-compose, public API) that strongly suggest people will run it as a service. Under that assumption it has several **critical issues**, the most important of which are:

1. **The HTTP API has no authentication enabled** — the `verify_token` dependency exists but is commented out on every router. Anyone who can reach the API can submit jobs, delete tasks, upload files, and call the LLM endpoints on the operator's dime.
2. **Arbitrary local file read** via the `custom_audio_file` parameter on `POST /api/v1/videos` — combined with #1, an unauthenticated attacker can probe for any file the process can read and (depending on what the audio pipeline does with the bytes) potentially exfiltrate content through the generated video output.
3. **CORS is configured as `allow_origins=["*"]` with `allow_credentials=True`** — the intent (any origin, with cookies) is invalid per the CORS spec and undermines what little browser protection might apply.
4. **The Docker image runs as root, `chmod 777`s its workdir, and deletes ImageMagick's path-policy hardening** — i.e. it intentionally re-enables a class of vulnerability ImageMagick disabled by default after "ImageTragick" (CVE-2016-3714).

The code is functional and not malicious — it's the standard "fast-moving demo app" security profile: built for `localhost`, slowly accumulating "production-ish" features without revisiting the threat model.

---

## Methodology

- Cloned the upstream repo and read the listed files end-to-end.
- Cross-referenced each claimed vulnerability against the actual code before including it.
- Where a control exists (e.g. `app/utils/file_security.py`), I traced every call site to confirm it's used.
- I did **not** run the app, fuzz endpoints, or test exploits. Findings are static; none are theoretical in the "this might exist somewhere" sense — each has a file:line citation.

---

## Critical findings

### C1. No authentication on the HTTP API
**Files:** `app/controllers/v1/video.py:37-38`, `app/controllers/v1/llm.py:14-15`

```python
# 认证依赖项
# router = new_router(dependencies=[Depends(base.verify_token)])
router = new_router()
```

The `verify_token` dependency in `app/controllers/base.py:21-31` reads an `x-api-key` header and compares it to `config.app["api_key"]`. It exists, works, and is never wired up. Every route on both `video` and `llm` routers is anonymous. The same goes for the Streamlit WebUI on port 8501 — no auth at all.

**Impact (with the documented `docker-compose` exposing `0.0.0.0:8080` and `0.0.0.0:8501`):**
- Anyone on the network can `POST /api/v1/videos` and burn the operator's Pexels/Pixabay/OpenAI quota.
- Anyone can `POST /api/v1/scripts` / `/api/v1/terms` and get free LLM inference billed to the operator.
- Anyone can `DELETE /api/v1/tasks/{id}` to wipe other users' work.
- Anyone can `POST /api/v1/musics` and `POST /api/v1/video_materials` to upload arbitrary `mp3`/`mp4`/`mov`/`avi`/`flv`/`mkv`/`jpg`/`jpeg`/`png` files (path traversal is blocked by `_sanitize_upload_filename`, but the upload itself is open).

**Fix:** un-comment the `dependencies=[Depends(base.verify_token)]` lines, ensure `config.app["api_key"]` has no default empty string (or fail-closed when empty), and document that the WebUI must be reverse-proxied behind auth.

---

### C2. Arbitrary local file inclusion via `custom_audio_file`
**Files:** `app/models/schema.py:87`, `app/services/task.py:87-124`

`VideoParams.custom_audio_file: Optional[str] = None` is a free-form path supplied in the request body. In `generate_audio`:

```python
custom_audio_file = getattr(params, "custom_audio_file", None)
if not custom_audio_file or not os.path.exists(custom_audio_file):
    ...
else:
    logger.info(f"using custom audio file: {custom_audio_file}")
    audio_duration = voice.get_audio_duration(custom_audio_file)
    ...
    return custom_audio_file, audio_duration, None
```

There is **no containment check** here. The file path is then used as `audio_file` through the rest of the pipeline (`combine_videos`, `generate_video`), so the bytes are read by the audio decoder and (if decoding succeeds for any portion) muxed into the output video at `tasks/{id}/final-N.mp4`, which the same anonymous attacker can then download via `GET /download/{task_id}/final-1.mp4`.

Even when decoding fails, `os.path.exists()` returning True vs False on attacker-supplied paths is a **file existence oracle**. Contrast this with `bgm_file` (`app/services/video.py:241-260`) and `video_materials[*].url` (`app/services/video.py:668-679`), both of which correctly call `file_security.resolve_path_within_directory()`.

**Fix:** apply the same `resolve_path_within_directory` containment that `bgm_file` uses, scoped to a dedicated `custom_audio` directory under `storage/`.

---

### C3. CORS configured with wildcard origin + credentials
**File:** `app/asgi.py:55-64`

```python
cors_allowed_origins_str = os.getenv("CORS_ALLOWED_ORIGINS", "")
origins = cors_allowed_origins_str.split(",") if cors_allowed_origins_str else ["*"]
app.add_middleware(
    CORSMiddleware,
    allow_origins=origins,
    allow_credentials=True,
    allow_methods=["*"],
    allow_headers=["*"],
)
```

The default with no env var set is `["*"]` + `allow_credentials=True`. Per the CORS spec, browsers will refuse to send credentials when the response says `Access-Control-Allow-Origin: *`, but Starlette/FastAPI's `CORSMiddleware` will reflect the request origin when `*` is set with credentials, which effectively turns this into "allow any origin with credentials." Combined with C1 (no auth, so credentials don't matter anyway), the practical impact is small — but the moment auth is fixed, this becomes a CSRF vector against the API.

**Fix:** require `CORS_ALLOWED_ORIGINS` to be set; refuse to start if it's empty or `*` while `allow_credentials=True`.

---

### C4. ImageMagick path policy intentionally disabled in the Docker image
**File:** `Dockerfile:48`

```dockerfile
RUN sed -i '/<policy domain="path" rights="none" pattern="@\*"/d' /etc/ImageMagick-6/policy.xml
```

This removes the `@*` path policy that ImageMagick ships disabled by default specifically to mitigate the "ImageTragick" CVE-2016-3714 family (which lets a crafted image file trigger ImageMagick to read/exec via `@filename` indirection). MoviePy uses ImageMagick for text rendering in the subtitle path; user-controlled `subtitle_position`, `text_fore_color`, etc. flow into that pipeline, as does user-controlled `font_name`. I did not find a path where the *content* of an attacker-controlled file is passed to ImageMagick (so this isn't immediately reachable), but the policy was disabled by ImageMagick maintainers as defense-in-depth, and re-enabling the attack surface to silence a runtime warning is a strict downgrade.

**Fix:** keep the policy in place; if MoviePy genuinely needs the `@` indirection, scope the exception to a specific known path instead of deleting the whole line.

---

## High findings

### H1. Container runs as root with `chmod 777` workdir and bind-mounts the host repo
**Files:** `Dockerfile:1-8`, `docker-compose.yml:1-23`

```dockerfile
FROM python:3.11-slim-bullseye
WORKDIR /MoneyPrinterTurbo
RUN chmod 777 /MoneyPrinterTurbo
```

No `USER` directive; the container's PID 1 is root. `docker-compose.yml` then bind-mounts the host's entire repo directory (`./:/MoneyPrinterTurbo`) into both `webui` and `api` containers. Any RCE inside the container (e.g. an ImageMagick or ffmpeg parser bug — both are large C codebases) becomes root write access to the host repo, including `config.toml` (which holds every API key the user configured) and `.git/`.

**Fix:** add a non-root user, drop the `chmod 777`, and mount only `config.toml` and `storage/` rather than the whole repo (the README's `docker run` example actually does this correctly; `docker-compose.yml` is the one over-mounting).

---

### H2. Streamlit WebUI exposes config-mutation surface with no auth
**Files:** `Dockerfile:65`, `docker-compose.yml:11-13`, `webui/Main.py:1150-line config editor` + `app/config/config.py:35-41`

The `webui` service publishes `0.0.0.0:8501` and runs `streamlit run` with `--server.enableCORS=True` (no auth flag). The Streamlit app uses `config.save_config()` to persist edits back to `config.toml`. The flag `--browser.serverAddress=127.0.0.1` only affects the URL Streamlit prints — the server itself binds 0.0.0.0 by default.

Result: anyone who can reach the WebUI can:
1. Read every API key (they're rendered in form inputs),
2. Change the LLM provider, API keys, base URLs, and endpoints,
3. Toggle `enable_g4f` (which then loads a reverse-engineered third-party library on the next request),
4. Submit jobs that bill the operator.

**Fix:** put Streamlit behind a reverse proxy that enforces auth, or use Streamlit's native auth (`st.experimental_user` with an IdP), and stop persisting secrets into a writable `config.toml` from a network-reachable UI.

---

### H3. `/tasks` static mount has `follow_symlink=True`
**File:** `app/asgi.py:67-69`

```python
app.mount(
    "/tasks", StaticFiles(directory=task_dir, html=True, follow_symlink=True), name=""
)
```

The download/stream endpoints (`/stream/...`, `/download/...`) use `file_security.resolve_path_within_directory()` which is symlink-safe (it uses `os.path.realpath` and `commonpath`). The `/tasks` static mount does not — it serves whatever the path resolves to. Right now there's no API that lets an unauthenticated attacker plant a symlink under `task_dir`, so this isn't directly exploitable, but it nullifies the symlink protection that exists everywhere else and is a footgun for any future endpoint that writes to `task_dir`.

**Fix:** drop `follow_symlink=True` unless there's a concrete reason. The downloads still work via the explicit `/download` and `/stream` routes, which already validate paths.

---

### H4. Race condition on the in-memory task counter
**File:** `app/controllers/manager/base_manager.py:22-58`

```python
def add_task(self, func, *args, **kwargs):
    with self.lock:
        if self.current_tasks < self.max_concurrent_tasks:
            self.execute_task(func, *args, **kwargs)   # spawns thread, does NOT increment yet
        else:
            ...

def run_task(self, func, *args, **kwargs):
    try:
        with self.lock:
            self.current_tasks += 1                    # increment happens HERE, in the new thread
        func(*args, **kwargs)
    finally:
        self.task_done()
```

`execute_task` only starts a thread; the increment of `current_tasks` happens inside the new thread's `run_task`, after `add_task` has already released the lock. Between those two moments, another `add_task` call observes the old (lower) `current_tasks` and also passes the gate. Under load, `max_concurrent_tasks` is a soft cap that can be exceeded — the more so the heavier the import-time work `func` does before the lock is acquired again.

**Fix:** increment `self.current_tasks` inside the same `with self.lock:` block in `add_task`, before starting the thread; decrement on failure.

---

### H5. Error messages and logs leak full configuration (incl. API keys)
**File:** `app/services/material.py:37-43`

```python
def get_api_key(cfg_key: str):
    api_keys = config.app.get(cfg_key)
    if not api_keys:
        raise ValueError(
            f"\n\n##### {cfg_key} is not set #####\n\nPlease set it in the config.toml file: {config.config_file}\n\n"
            f"{utils.to_json(config.app)}"
        )
```

When *any* required key is missing, the entire `config.app` dict is dumped into the exception message, which is then handed up the FastAPI exception handler (`app/asgi.py:18-22`) and returned to the unauthenticated caller. That dict contains every other configured API key (OpenAI, Moonshot, Azure, DeepSeek, Gemini, Qwen, MiniMax, Pixabay, upload-post, etc.) — i.e. failing to set one key leaks all the others. The same dump is also written to logs.

**Fix:** raise with just the missing key name; never include the full config in error messages or logs.

---

### H6. SSRF / IMDS reach from `requests.get` on user-influenced URLs
**Files:** `app/services/material.py:74-80, 133-135, 190-199`

`save_video()` does an unrestricted `requests.get(video_url, proxies=config.proxy, ...)` against URLs returned from the Pexels/Pixabay search APIs. Those URLs are presumed-trusted, but:

- Search terms (`video_terms`) are attacker-controllable (no auth — C1) and can be chosen to influence what URLs the upstream API returns,
- More directly, if the Pexels/Pixabay accounts are compromised or the upstream APIs are spoofed (no TLS pinning, and `tls_verify` can be disabled in config), an attacker can redirect the downloader to `http://169.254.169.254/...` (cloud IMDS), `http://localhost:6379` (the operator's own Redis), or any internal HTTP service the container can reach.

There is no allow-list on resolved IPs and no block of RFC1918 / link-local destinations.

**Fix:** validate that downloaded URLs resolve to public IPs only (use a resolver hook that rejects private/link-local/loopback ranges) and restrict allowed hostnames to the known CDN domains for each provider.

---

### H7. `g4f` ships as a hard pinned dependency despite being feature-flagged
**Files:** `requirements.txt:12`, `app/services/llm.py:60-89`

The code correctly gates `g4f` behind `enable_g4f=true` in config and emits a warning, but `requirements.txt` still pins `g4f==0.5.2.2`. `g4f` is a wrapper around reverse-engineered third-party endpoints with a long history of supply-chain noise (random network calls on import, vendored binaries, etc.). Keeping it in the default install means every operator pulls and trusts that package whether they enable the provider or not. `pyproject.toml`/`uv.lock` move it to an extra (`uv sync --extra g4f`); `requirements.txt` should match.

**Fix:** remove `g4f` from `requirements.txt`; document the optional-extra install path only.

---

### H8. Redis password embedded in a URL string
**Files:** `app/controllers/v1/video.py:48`, `app/services/state.py:160-165`

```python
redis_url = f"redis://:{_redis_password}@{_redis_host}:{_redis_port}/{_redis_db}"
```

Constructed at module import time, then passed to `redis.Redis.from_url(redis_url)`. The plaintext password lives in `redis_url` for the lifetime of the module and ends up wherever the string is logged (currently nowhere I could see, but any `logger.exception()` that captures locals — e.g. Sentry — would pick it up). Worse, the URL form doesn't URL-encode the password, so any password containing `@`, `/`, `:`, or `?` will silently corrupt the connection string.

**Fix:** call `redis.Redis(host=..., port=..., password=...)` directly the way `RedisState` already does (`app/services/state.py:62-66`).

---

## Medium findings

### M1. `enqueue` uses Pydantic v1 `.dict()` on a v2 model
**File:** `app/controllers/manager/redis_manager.py:35-37`

```python
task_with_serializable_params["kwargs"]["params"] = task["kwargs"]["params"].dict()
```

`VideoParams` is a Pydantic v2 `BaseModel` (`pyproject.toml` and `uv.lock` pin Pydantic v2). `.dict()` is deprecated in v2 and emits a warning on every enqueue; `.model_dump()` is the supported call. Functionally it still works today but is brittle against the next Pydantic major.

---

### M2. Range header parsing crashes on malformed input
**File:** `app/controllers/v1/video.py:350-358`

```python
if range_header:
    range_ = range_header.split("bytes=")[1]
    start, end = [int(part) if part else None for part in range_.split("-")]
```

- `Range: bytes=` (empty) → `split("-")` → `[""]` → unpacking to `start, end` raises `ValueError`.
- `Range: bytes=abc-def` → `int("abc")` raises `ValueError`.
- `Range: bytes=100-50` (start > end) → not validated; `length` goes negative and `f.read(min(4096, negative))` is undefined.
- Negative `start`/`end` from suffix form (`bytes=-500`) handled, but no check that the result is in `[0, video_size)`.

Errors here surface as 500s rather than 416s. Not a security bug, just a robustness one.

---

### M3. `ffmpeg-concat-list.txt` is shared per task directory, not unique
**File:** `app/services/video.py:90-129`

```python
concat_list_file = os.path.join(output_dir, "ffmpeg-concat-list.txt")
```

If `combine_videos` is called twice in parallel for the same task (the current pipeline doesn't, but the same `output_dir` is used for every concat call within a task that has `video_count > 1`), the second call's concat list will clobber the first's. Less serious than the audit-agent suggested because the pipeline is sequential, but if `video_count > 1` ever moves to a thread pool this becomes wrong silently.

**Fix:** use `tempfile.NamedTemporaryFile(dir=output_dir, suffix=".txt", delete=False)`.

---

### M4. `voice.py` is 2188 lines, mixing TTS, voice metadata, and subtitle generation
**File:** `app/services/voice.py`

A pure architecture finding: this file is twice the size of the next-largest module and combines unrelated concerns. It's hard to review for bugs (which is part of why I'm not citing specific ones in it — I spot-read it, but a full review would need its own audit). Splitting into `tts.py`, `voices.py`, `subtitles_edge.py` would make future changes (and security review) tractable.

---

### M5. `webui/Main.py` is a 1150-line single file
**File:** `webui/Main.py`

Same architectural smell as M4: every form, every callback, and the config-mutation logic live in one Streamlit script. Especially relevant because the same file holds API-key form inputs *and* the save logic that writes them back to disk.

---

### M6. Pinned dependency drift between `requirements.txt` and `pyproject.toml`
**File:** `requirements.txt`

The header comment says "primary deps moved to pyproject.toml / uv.lock" but the file is still committed with hard pins (e.g. `openai==1.56.1`, `fastapi==0.115.6`, `moviepy==2.1.2`, `streamlit==1.45.0`). Anyone installing via `pip install -r requirements.txt` instead of `uv sync` gets a different and likely older dependency tree than CI/tests use. Either delete `requirements.txt` and document `uv sync` as the only install path, or regenerate it from the lockfile on each release.

---

### M7. `MaterialInfo` is a Pydantic dataclass holding a free-form `url: str`
**File:** `app/models/schema.py:51-55`

```python
@pydantic.dataclasses.dataclass(config=_Config)
class MaterialInfo:
    provider: str = "pexels"
    url: str = ""
    duration: int = 0
```

The containment check in `video.preprocess_video` (file:670) does the right thing: any path that escapes `local_videos_dir` is logged and skipped. But the model accepts `provider: str` with no enum constraint, so the only thing keeping the local-vs-remote branching honest is the `if params.video_source == "local"` check in `app/services/task.py:165`. If a future caller ever uses `MaterialInfo` outside that branch (e.g. for "cached" materials), the model itself won't push back. Worth promoting `provider` to an enum and `url` to a validated `FilePath`/`HttpUrl` union.

---

### M8. Tests are integration-only and hit live external services
**Files:** `test/services/test_task.py`, `test/services/test_voice.py`, `test/services/test_material.py`

`test_task.py` calls `tm.start(task_id=..., params=...)` with real `voice_name="zh-CN-XiaoxiaoNeural-Female"` — this requires Edge TTS connectivity and writes real files to `storage/`. None of the security-relevant logic (`file_security`, the manager's concurrency cap, the upload sanitizer, the path containment) has a unit test. As a result, regressions in the security path won't be caught by CI.

**Fix:** add unit tests for `app/utils/file_security.py` covering symlink escape, absolute path, `..` traversal, Windows drive-letter cases (the code handles that branch — `ValueError` from `commonpath`), and a negative test for the `custom_audio_file` path once C2 is fixed.

---

## Low findings

### L1. Empty `__init__.py` for `app/services/` and `app/models/` does nothing
Minor: not all modules need the package marker file, but it's consistent with the rest of the tree.

### L2. `_api_key_counter` is a module-global with a lock but not reset
**File:** `app/services/material.py:15-52`

The counter never wraps cleanly because it's a free integer; under multi-day uptime it grows unboundedly. Cosmetic but worth a `_api_key_counter % len(api_keys)` *before* increment plus a periodic reset.

### L3. `# noqa`-less wildcard exception handlers throughout
Pattern across `video.py`, `material.py`, `voice.py` of `except Exception as e: logger.warning(...)` that swallows everything including `KeyboardInterrupt` would-be `SystemExit` (Python 3's `Exception` doesn't include those, so this is fine in practice, but `except:` bare is used in `app/services/voice.py` in several places and is worth cleaning up).

### L4. `print()` debug statements left in `webui/Main.py:13-15`
```python
print("******** sys.path ********")
print(sys.path)
```
Cosmetic but pollutes the Streamlit launch log every time.

### L5. The deletion endpoint trusts that `task_id` is a UUID
**File:** `app/controllers/v1/video.py:220-235`
`task_id` is taken from the path and used as `os.path.join(tasks_dir, task_id)` then `shutil.rmtree`. There's no UUID validation; `task_id = "../../something/inside/tasks_dir"` would actually escape because `os.path.join` doesn't normalize. In practice `tasks_dir` only contains UUID-named subfolders so a traversal targets nothing useful, but the same `tasks_dir` path is used elsewhere with `file_security.resolve_path_within_directory` — applying it here too would be cheap and consistent.

### L6. `delete_files()` silently swallows missing-file errors
**File:** `app/services/video.py:227-235`
Logs at `debug` level; the cleanup never reports back. Fine for happy-path but masks bugs if a temp file path is computed wrong.

---

## Observations that are not findings

- **`app/utils/file_security.py` is correctly implemented.** It uses `os.path.realpath` + `os.path.commonpath` (rather than string-prefix comparison), handles Windows cross-drive `ValueError`, and is called from the right places (`bgm_file`, `video_materials[*].url`, `/stream` and `/download` endpoints). The one gap is C2 (`custom_audio_file`).
- **The Range-header path on `/stream/...` does validate `file_path` first** — C2 isn't reachable via that endpoint, only via `custom_audio_file` in the request body.
- **`ast.literal_eval` on Redis data (`app/services/state.py:145`)** is safe by construction — `literal_eval` cannot execute code; the worst case is a value that doesn't parse cleanly, which is caught.

---

## Prioritized remediation list

1. **(C1)** Re-enable `verify_token` on both routers; refuse to start the API if `config.app["api_key"]` is empty. (~5 lines)
2. **(C2)** Containment-check `custom_audio_file` via `file_security.resolve_path_within_directory` against a dedicated upload directory. (~10 lines)
3. **(H5)** Stop dumping `config.app` in `ValueError` messages and logs. (~3 lines)
4. **(H4)** Move `current_tasks += 1` into the lock held by `add_task`. (~5 lines)
5. **(C3)** Refuse to start when CORS is `*` + credentials.
6. **(H1)** Add `USER` directive to Dockerfile, drop `chmod 777`, and trim `docker-compose.yml` bind mounts to `config.toml` + `storage/` only.
7. **(C4)** Restore ImageMagick's path policy (or scope the override to a known font directory).
8. **(H2)** Put the WebUI behind auth; stop persisting secrets via the WebUI form.
9. **(H7)** Drop `g4f` from `requirements.txt`.
10. **(H8)** Switch to keyword-argument Redis construction.
11. (H3, M1–M8, L1–L6) Cleanup pass.

Steps 1–4 are the changes that meaningfully move the system out of "do not expose to a network" territory.
