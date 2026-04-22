# Isso Comment Server – Rust Porting Reference

Version: 0.14.0  
Wire Protocol: SQLite3 database + REST JSON API  
Original Python source: upstream `isso/` tree — see
[`isso-comments/isso`](https://github.com/isso-comments/isso).

> **Status**: This document was written as a wire-compatibility spec
> *before* the port. The `isso/*.py` paths cited below no longer exist in
> this repository; they reference the upstream Python implementation at
> the time of the port. The Rust implementation is under `src/`
> and wire-compat is now enforced by tests rather than by this document
> — see `tests/schema_compat.rs` and the `probes_match_python_for_known_keys`
> / `default_pbkdf2_matches_python` tests in the `bloomfilter` and `hash`
> modules.

---

## 1. Database Schema

**Location**: `isso/db/__init__.py` (SQLite3 class), `isso/db/comments.py`, `isso/db/threads.py`, `isso/db/preferences.py`

### Database Version

- **Current**: `MAX_VERSION = 5` (`isso/db/__init__.py:25`)
- **Migrations**: Versions 0→5 defined in `migrate()` method

### Tables

#### `threads` (isso/db/threads.py:11-15)

```sql
CREATE TABLE IF NOT EXISTS threads (
    id INTEGER PRIMARY KEY,
    uri VARCHAR(256) UNIQUE,
    title VARCHAR(256)
);
```

**Fields**:
- `id`: Auto-increment primary key
- `uri`: Thread URI, must be unique
- `title`: Thread title

#### `comments` (isso/db/comments.py:50-69)

```sql
CREATE TABLE IF NOT EXISTS comments (
    tid REFERENCES threads(id),
    id INTEGER PRIMARY KEY,
    parent INTEGER,
    created FLOAT NOT NULL,
    modified FLOAT,
    mode INTEGER,
    remote_addr VARCHAR,
    text VARCHAR NOT NULL,
    author VARCHAR,
    email VARCHAR,
    website VARCHAR,
    likes INTEGER DEFAULT 0,
    dislikes INTEGER DEFAULT 0,
    voters BLOB NOT NULL,
    notification INTEGER DEFAULT 0
);
```

**Fields (isso/db/comments.py:28-45)**:
- `tid`: Foreign key to threads.id
- `id`: Auto-increment primary key (unique comment ID)
- `parent`: References comment.id (NULL for top-level)
- `created`: UNIX timestamp (float), set at comment creation
- `modified`: UNIX timestamp (float) or NULL, set on edit
- `mode`: Comment status (see below)
- `remote_addr`: Anonymized IPv4/IPv6 address
- `text`: Raw comment text (Markdown), NOT NULL (v5+)
- `author`: Commenter's name or NULL
- `email`: Commenter's email or NULL
- `website`: Commenter's website URL or NULL
- `likes`: Like counter, default 0
- `dislikes`: Dislike counter, default 0
- `voters`: Bloomfilter BLOB (see voters section)
- `notification`: Boolean flag for reply notifications (0/1)

**Mode values** (isso/db/comments.py:34-35):
- `1`: Accepted/published
- `2`: Pending moderation
- `4`: Soft-deleted (referenced by replies, text/author/website cleared)

#### `preferences` (isso/db/preferences.py:14)

```sql
CREATE TABLE IF NOT EXISTS preferences (
    key VARCHAR PRIMARY KEY,
    value VARCHAR
);
```

**Default rows**:
- `session-key`: Random 48-hex-char string (24 bytes → hex), generated via `binascii.b2a_hex(os.urandom(24)).decode("utf-8")`

### Triggers (isso/db/__init__.py:45-53)

```sql
CREATE TRIGGER IF NOT EXISTS remove_stale_threads
AFTER DELETE ON comments
BEGIN
    DELETE FROM threads WHERE id NOT IN (SELECT tid FROM comments);
END;
```

Automatically removes threads when their last comment is deleted.

### Migrations

**v0→v1** (isso/db/__init__.py:74-91): Re-initialize voters bloomfilter due to signature bug.

**v1→v2** (isso/db/__init__.py:94-112): Move [general] session-key to database preferences.

**v2→v3** (isso/db/__init__.py:114-148): Limit nesting to 1 level (all nested comments reparented to root).

**v3→v4** (isso/db/__init__.py:150-153, migrate_to_version_4): Add `notification` column to comments via `ALTER TABLE`.

**v4→v5** (isso/db/__init__.py:155-188): Create new comments table with `text NOT NULL` constraint, copy data, swap tables.

---

## 2. Comment Model & Fields

### Full Comment Record

Fields returned by `Comments.fetch()` and `Comments.get()` (isso/db/comments.py:28-45):

```python
{
    "tid": int,                    # Thread ID
    "id": int,                     # Comment ID
    "parent": int | None,          # Parent comment ID or NULL
    "created": float,              # UNIX timestamp (seconds)
    "modified": float | None,      # UNIX timestamp or NULL
    "mode": 1 | 2 | 4,             # Status: 1=accepted, 2=pending, 4=deleted
    "remote_addr": str,            # Anonymized IP (e.g. "192.168.1.0" for IPv4)
    "text": str,                   # Raw Markdown text
    "author": str | None,          # Author name or NULL
    "email": str | None,           # Email or NULL
    "website": str | None,         # Website URL or NULL
    "likes": int,                  # Like count (default 0)
    "dislikes": int,               # Dislike count (default 0)
    "voters": bytes,               # Bloomfilter serialized as BLOB
    "notification": int            # 0 or 1 (reply notification opt-in)
}
```

### Voters Bloomfilter

**Location**: isso/utils/__init__.py:37-92

**Specification**:
- **Type**: Bloomfilter (probabilistic set membership)
- **Array size**: 256 bytes (2048 bits)
- **Hash functions**: 11 SHA256-based probes
- **False-positive rate**: ~1e-05 for <80 elements, 1e-04 for <105, 1e-03 for <142
- **Serialization**: Raw bytearray(256), stored as BLOB in SQLite

**Implementation** (isso/utils/__init__.py:68-92):
```python
class Bloomfilter:
    def __init__(self, array=None, elements=0, iterable=()):
        self.array = array or bytearray(256)
        self.elements = elements
        self.k = 11  # Number of hash functions
        self.m = len(self.array) * 8  # 2048 bits

    def get_probes(self, key):
        """Generate 11 bit positions for a key via SHA256"""
        h = int(hashlib.sha256(key.encode()).hexdigest(), 16)
        for _ in range(self.k):
            yield h & self.m - 1
            h >>= self.k

    def add(self, key):
        """Add key (IP address) to filter"""
        for i in self.get_probes(key):
            self.array[i // 8] |= 2 ** (i % 8)
        self.elements += 1

    def __contains__(self, key):
        """Check if key exists (may have false positives)"""
        return all(self.array[i // 8] & (2 ** (i % 8)) for i in self.get_probes(key))
```

**Usage in comments.py**:
- Initialize on comment creation: `Bloomfilter(iterable=[remote_addr]).array` → stored as `memoryview(bf.array)` (isso/db/comments.py:127)
- Check on vote: `Bloomfilter(bytearray(voters), likes + dislikes)` (isso/db/comments.py:378)
- Add on vote: `bf.add(remote_addr)` then update: `memoryview(bf.array)` (isso/db/comments.py:386, 393)

---

## 3. Hashing

**Location**: isso/utils/hash.py

### Hash Functions

#### Base `Hash` class

```python
class Hash:
    func = None  # Hash function name (e.g. "sha1", "md5")
    salt = b"Eech7co8Ohloopo9Ol6baimi"  # Default salt (25 bytes)

    def hash(self, val: bytes) -> bytes:
        """Compute hash(salt + val)"""
        return hashlib.new(self.func).update(val).digest()

    def uhash(self, val: str) -> str:
        """Unicode hash: return hex string"""
        return codecs.encode(self.hash(val.encode("utf-8")), "hex_codec").decode("utf-8")
```

#### `PBKDF2` class

```python
class PBKDF2(Hash):
    def __init__(self, salt=None, iterations=1000, dklen=6, func="sha1"):
        self.iterations = iterations  # Iteration count
        self.dklen = dklen              # Output length in bytes
        self.func = func                # Hash function

    def compute(self, val: bytes) -> bytes:
        """PBKDF2 with specified iterations"""
        return pbkdf2_hmac(
            hash_name=self.func,
            password=val,
            salt=self.salt,
            iterations=self.iterations,
            dklen=self.dklen
        )
```

### Default Configuration (isso/isso.cfg:244-261)

```ini
[hash]
salt = Eech7co8Ohloopo9Ol6baimi
algorithm = pbkdf2
```

**Full default**: `pbkdf2:1000:6:sha1` (isso/utils/hash.py:59)
- Iterations: 1000
- DKlen (output bytes): 6
- Function: sha1

### Hash Factory (isso/utils/hash.py:70-91)

```python
def new(conf):
    """Create hash function from config"""
    algorithm = conf.get("algorithm")  # e.g. "pbkdf2", "sha1", "none"
    salt = conf.get("salt").encode("utf-8")

    if algorithm == "none":
        return Hash(salt, None)  # No hashing
    elif algorithm.startswith("pbkdf2"):
        # Parse "pbkdf2:iterations:dklen:func"
        kwargs = {}
        tail = algorithm.partition(":")[2]
        for func, key in ((int, "iterations"), (int, "dklen"), (str, "func")):
            head, _, tail = tail.partition(":")
            if not head: break
            kwargs[key] = func(head)
        return PBKDF2(salt, **kwargs)
    else:
        return Hash(salt, algorithm)  # Direct hashlib function
```

### Usage in API (isso/views/comments.py:179, 403)

```python
# Hash computed from email or remote_addr
rv["hash"] = self.hash(rv["email"] or rv["remote_addr"])

# Computation:
hasher = hash.new(conf.section("hash"))
hash_result = hasher.uhash(value)  # Returns hex string (e.g. "e644f6ee43c0" for SHA1)
```

Output format: Hexadecimal string (12 chars for sha1 with dklen=6 → 6 bytes × 2 hex chars).

---

## 4. HTTP API

**Base endpoint**: `/` (comment root)  
**Routes** (isso/views/comments.py:156-175):

### POST /new (Create comment)

**Request**:
- Query: `uri` (required, thread URI)
- Headers: `Content-Type: application/json`, CSRF check (no form-encoded content)
- Body:
  ```json
  {
    "text": "...",           // Required, 3-65535 chars
    "author": "...",        // Optional
    "email": "...",         // Optional, max 254 chars
    "website": "...",       // Optional, max 254 chars, must match URL regex
    "parent": 15,           // Optional parent comment ID
    "title": "...",         // Optional thread title (required if creating new thread and URI not fetchable)
    "notification": 0 | 1   // Optional reply notification opt-in
  }
  ```
- Cookies: None required

**Response** (isso/views/comments.py:315-332):
- Status: 201 (accepted) or 202 (pending moderation)
- Headers: 
  ```
  Set-Cookie: {id}={signed_token}; Expires=...; Max-Age=900; Path=/; SameSite=Lax|None
  X-Set-Cookie: isso-{id}={signed_token}; Expires=...; Max-Age=900; Path=/; SameSite=None
  ```
  Where `signed_token = URLSafeTimedSerializer.dumps([id, sha1(text)])`
- Body (isso/views/comments.py:135-150):
  ```json
  {
    "id": 23,
    "parent": 15,
    "text": "<p>...</p>",      // Rendered HTML
    "author": "...",
    "website": "...",
    "mode": 1 | 2,
    "created": 1464940838.254393,
    "modified": null,
    "likes": 0,
    "dislikes": 0,
    "hash": "e644f6ee43c0",
    "gravatar_image": "...",  // If gravatar enabled
    "notification": 0 | 1
  }
  ```

**Validation** (isso/views/comments.py:221-247):
- Text: required, 3–65535 chars
- Parent: integer or null
- Author, website, email: strings or null
- Email: max 254 chars
- Website: max 254 chars, must pass URL regex (isso/views/comments.py:33-44)

**Cookie token format**:
- Signed with itsdangerous `URLSafeTimedSerializer`
- Payload: `[comment_id: int, text_hash: str]`
- Default max_age: 900 seconds (15 min)

### GET / (Fetch comments for thread)

**Request**:
- Query:
  ```
  uri={uri}                           // Required
  plain=0|1                           // Optional (0=render HTML, 1=return raw text)
  parent={id}                         // Optional (fetch only replies to this comment)
  limit={n}                           // Optional (max top-level comments)
  nested_limit={n}                    // Optional (max replies per comment)
  offset={n}                          // Optional (pagination offset, requires limit)
  after={unix_timestamp}              // Optional (fetch only after timestamp)
  sort=oldest|newest|upvotes          // Optional (default: oldest)
  ```

**Response** (isso/views/comments.py:1003-1009):
- Status: 200
- Body:
  ```json
  {
    "id": null,                 // Root comment ID (null for top-level)
    "total_replies": 14,        // Total comments matching filter
    "hidden_replies": 0,        // Omitted due to limit
    "replies": [
      {
        "id": 1,
        "parent": null,
        "text": "...",
        "author": "...",
        "website": "...",
        "mode": 1,
        "created": 1464818460.732863,
        "modified": null,
        "likes": 2,
        "dislikes": 2,
        "hash": "1cb6cc0309a2",
        "gravatar_image": "...",  // If gravatar enabled
        "notification": 0,
        "total_replies": 1,
        "hidden_replies": 0,
        "replies": [
          {
            "id": 2,
            "parent": 1,
            ...
          }
        ]
      }
    ],
    "config": {
      "reply-to-self": false,
      "require-email": false,
      "require-author": false,
      "reply-notifications": false,
      "gravatar": false,
      "avatar": false,
      "feed": false
    }
  }
  ```

**Sort mapping** (isso/views/comments.py:940-955):
- `newest`: ORDER BY created DESC
- `oldest`: ORDER BY created ASC
- `upvotes`: ORDER BY (likes - dislikes) DESC

### GET /id/<id> (View comment)

**Request**:
- Query: `plain=0|1`
- Cookies: `{id}={signed_token}` (required, must be valid and not expired)

**Response**:
- Status: 200 or 403 (Forbidden if cookie invalid/expired)
- Body: Comment object (same as fetch)

### PUT /id/<id> (Edit comment)

**Request**:
- Query: None
- Headers: `Content-Type: application/json`, CSRF check
- Cookies: `{id}={signed_token}` (must match and verify text hash)
- Body:
  ```json
  {
    "text": "...",          // Required
    "author": "...",       // Optional
    "website": "..."       // Optional
  }
  ```

**Response**:
- Status: 200
- Headers: Set-Cookie with new token (contains updated text hash)
- Body: Updated comment object with new `modified` timestamp

### DELETE /id/<id> (Delete comment)

**Request**:
- Headers: CSRF check
- Cookies: `{id}={signed_token}`

**Response**:
- Status: 200
- Headers: Set-Cookie with expires=0, max-age=0 (delete cookie)
- Body: 
  - `null` if comment had no replies (hard deleted)
  - Comment object with mode=4, empty text/author/website if referenced (soft-deleted)

### POST /id/<id>/like, /dislike (Vote)

**Request**:
- Headers: CSRF check (Content-Type: application/json)
- No body, no cookies required

**Response**:
- Status: 200
- Body:
  ```json
  {
    "likes": 5,
    "dislikes": 2
  }
  ```

**Voting rules** (isso/db/comments.py:359-398):
- Max 142 total votes per comment
- One vote per IP (checked via bloomfilter)
- Vote count updated atomically with bloomfilter update

### POST /count (Count comments)

**Request**:
- Body: JSON array of URIs
  ```json
  ["/blog/firstPost.html", "/blog/post2.html"]
  ```

**Response**:
- Status: 200
- Body: JSON array of counts (in same order)
  ```json
  [2, 18]
  ```

Only counts published comments (mode=1).

### GET /feed (Atom/RSS feed)

**Request**:
- Query: `uri={uri}`
- Requires `[rss] base` configured

**Response**:
- Status: 200
- Content-Type: `text/xml`
- Body: Atom feed (limited to 100 comments by default, isso/isso.cfg:273)

### GET /id/<id>/<action>/<key> (Moderation GET)

**Request**:
- Action: `activate`, `edit`, `delete`
- Key: Signed comment ID via `isso.sign(comment_id)`

**Response**:
- Status: 200
- Content-Type: `text/html`
- Body: Confirmation dialog with JavaScript POST fallback

### POST /id/<id>/<action>/<key> (Moderation POST)

**Request**:
- Headers: CSRF check
- Action: `activate`, `edit`, `delete`
- For `edit`: JSON body with text, author, website

**Response**:
- Status: 200
- Body: Plain text ("Comment has been activated/deleted") or JSON (for edit)

### GET /id/<id>/unsubscribe/<email>/<key>

**Request**:
- Email: URL-encoded email
- Key: Signed `["unsubscribe", email]` via `isso.sign(..., max_age=2**32)`

**Response**:
- Status: 200
- Content-Type: `text/html`
- Body: Confirmation HTML

### POST /preview (Markdown preview)

**Request**:
- Body: `{"text": "..."}`

**Response**:
- Status: 200
- Body: `{"text": "<p>...</p>"}`

### POST /login/ (Admin login)

**Request**:
- Body: `{"password": "..."}`

**Response**:
- Status: 200/403
- Headers: Set-Cookie with admin session token

### GET /admin/ (Admin interface)

**Request**:
- Cookies: Admin session cookie required

**Response**:
- Status: 200
- Content-Type: `text/html`
- Body: Admin dashboard HTML

### GET /config (Server config)

**Request**: None

**Response**:
- Status: 200
- Body: Public config object (same as `replies[0].config`)

---

## 5. Configuration

**File**: isso/isso.cfg (default config)

### [general]

- `dbpath` (str): SQLite database path, default `/tmp/comments.db`
- `name` (str): Multi-site name, default empty
- `host` (str, multi-line): Whitelisted origin(s), required
- `max-age` (timedelta): Edit/delete window, default `15m` (900 sec)
- `notify` (str, comma-sep): Notification backends (`stdout`, `smtp`)
- `reply-notifications` (bool): Enable reply email notifications
- `log-file` (str): Log file path, default empty (stdout)
- `gravatar` (bool): Include gravatar_image in JSON, default false
- `gravatar-url` (str): Gravatar template, default `https://www.gravatar.com/avatar/{}?d=identicon&s=55`
- `latest-enabled` (bool): Enable /latest endpoint, default false

### [admin]

- `enabled` (bool): Enable admin interface, default false
- `password` (str): Admin password

### [moderation]

- `enabled` (bool): Enable moderation queue, default false
- `approve-if-email-previously-approved` (bool): Auto-approve if email has previous approved comments (6-month window)
- `purge-after` (timedelta): Delete unmoderated comments older than this, default `30d`

### [server]

- `listen` (str): WSGI server address, default `http://localhost:8080`
- `public-endpoint` (str): Public URL, auto-detected if empty
- `reload` (bool): Auto-reload on source changes, default false
- `profile` (bool): Show profiling stats, default false
- `trusted-proxies` (str, multi-line): Reverse proxy IPs for X-Forwarded-For
- `samesite` (str): Cookie SameSite value (`None`, `Lax`, `Strict`), auto-set if empty

### [smtp]

- `username`, `password` (str): SMTP credentials
- `host` (str): SMTP server, default `localhost`
- `port` (int): SMTP port, default 587
- `security` (str): `none`, `starttls`, `ssl`, default `starttls`
- `to` (str): Recipient email
- `from` (str): Sender email
- `timeout` (int): SMTP timeout sec, default 10

### [guard]

- `enabled` (bool): Enable spam protection, default true
- `ratelimit` (int): Max comments/min per IP, default 2
- `direct-reply` (int): Max direct replies per IP per thread, default 3
- `reply-to-self` (bool): Allow replying to own comment during edit window, default false
- `require-author` (bool): Require author field, default false
- `require-email` (bool): Require email field, default false

### [markup]

- `renderer` (str): `mistune` or `misaka`, default `mistune`
- `allowed-elements` (str, comma-sep): Extra HTML tags to allow
- `allowed-attributes` (str, comma-sep): Extra attributes to allow

### [markup.mistune]

- `plugins` (str, comma-sep): Mistune plugins, default `strikethrough, subscript, superscript`
- `parameters` (str, comma-sep): Renderer parameters, default `escape, hard_wrap`

### [markup.misaka] (deprecated)

- `options` (str, comma-sep): Misaka extensions
- `flags` (str, comma-sep): HTML rendering flags

### [hash]

- `salt` (str): Hash salt, default `Eech7co8Ohloopo9Ol6baimi`
- `algorithm` (str): `pbkdf2`, `sha1`, `md5`, `none`, default `pbkdf2`

### [rss]

- `base` (str): Base URL for feed links, default empty (disables feeds)
- `limit` (int): Max comments per feed, default 100

---

## 6. Markdown & HTML Sanitization

**Rendering pipeline** (isso/html/__init__.py, isso/html/mistune.py):

### Markdown Parser

**Default**: Mistune 3.1+ (isso/isso.cfg:208, isso/html/__init__.py:90-93)

**Mistune config** (isso/isso.cfg:237-241):
```ini
[markup.mistune]
plugins = strikethrough, subscript, superscript
parameters = escape, hard_wrap
```

**Parsing** (isso/html/mistune.py):
```python
mistune.create_markdown(
    escape=True,              # Escape HTML
    hard_wrap=True,           # Convert \n → <br>
    plugins=["strikethrough", "subscript", "superscript"]
)
```

### HTML Sanitizer

**Library**: bleach 4.0+

**Default allowed tags** (isso/html/__init__.py:23-51):
```
a, p, hr, br, ol, ul, li, pre, code, blockquote, del, ins,
strong, em, h1, h2, h3, h4, h5, h6, sub, sup, table, thead,
tbody, th, td
```

Plus any in `[markup] allowed-elements`

**Default allowed attributes** (isso/html/__init__.py:54):
- `a`: `href`
- `table`: `align`
- `code`: `class` (if matches `^language-[a-zA-Z0-9]{1,20}$`)
- All tags: `align` (global)

Plus any in `[markup] allowed-attributes`

**Link processing** (isso/html/__init__.py:59-83):
- Existing links (not new): prepend `rel="nofollow noopener"` if not present
- Skip mailto: links
- Strip tag brackets for bleach.linkifier

**Output** (isso/html/markdown.py:16-20):
```python
def render(text):
    rv = self._render(text).rstrip("\n")
    if not rv.startswith("<p>") and not rv.endswith("</p>"):
        rv = "<p>" + rv + "</p>"
    return rv
```

Renders to HTML with guaranteed `<p>` wrapper.

---

## 7. Rate Limiting / Guard

**Location**: isso/db/spam.py (Guard class)

**Configuration** (isso/isso.cfg:170-196):

```ini
[guard]
enabled = true
ratelimit = 2              # Max new comments per minute per IP (/24 for IPv4, /48 for IPv6)
direct-reply = 3           # Max direct replies (parent=NULL) per IP per thread
reply-to-self = false      # Allow reply to own comment during edit window
require-author = false     # Require author field
require-email = false      # Require email field
```

**Validation** (isso/db/spam.py:16-72):

```python
def validate(uri, comment):
    """Return (valid: bool, reason: str)"""
    # 1. Rate limit: max {ratelimit} comments in 60 seconds
    rv = db.execute(
        "SELECT id FROM comments WHERE remote_addr = ? AND ? - created < 60",
        (comment["remote_addr"], time.time())
    ).fetchall()
    if len(rv) >= self.conf.getint("ratelimit"):
        return False, "ratelimit exceeded"

    # 2. Direct reply limit: max {direct-reply} top-level comments per IP
    if comment["parent"] is None:
        rv = db.execute(
            "SELECT id FROM comments WHERE tid = ... AND remote_addr = ? AND parent IS NULL",
            ...
        ).fetchall()
        if len(rv) >= self.conf.getint("direct-reply"):
            return False, "N direct responses to {uri}"

    # 3. Reply-to-self check: block if replying to own recent comment
    # unless reply-to-self enabled
    if not self.conf.getboolean("reply-to-self"):
        rv = db.execute(
            "SELECT id FROM comments WHERE remote_addr = ? AND id = ? AND ? - created < ?",
            (comment["remote_addr"], comment["parent"], time.time(), self.max_age)
        ).fetchall()
        if len(rv) > 0:
            return False, "edit time frame is still open"

    # 4. Require author/email if configured
    if self.conf.getboolean("require-email") and not comment.get("email"):
        return False, "email address required"
    if self.conf.getboolean("require-author") and not comment.get("author"):
        return False, "author required"

    return True, ""
```

---

## 8. Email Notifications & Moderation

**Location**: isso/ext/notifications.py (SMTP class)

### When Emails Are Sent

**1. New comment (admin notification)**
- Event: `comments.new:after-save` (isso/ext/notifications.py:87)
- Condition: `[general] notify` contains `smtp` or `SMTP`
- Subject: "New comment posted" or "New comment posted on {title}"
- Body: Author, text, IP, link, delete/activate URLs

**2. Comment activation (reply notification)**
- Event: `comments.activate` (isso/ext/notifications.py:88)
- Condition: `[general] reply-notifications` enabled AND comment is published (mode=1)
- Recipient: Email of parent comment author (if subscribed)
- Subject: "Re: {title}" or "Re: comment"
- Body: Reply author, text, unsubscribe link

### Email Headers

**List-Unsubscribe** (isso/ext/notifications.py:91-94):
```
List-Unsubscribe: <{public_endpoint}/id/{parent_id}/unsubscribe/{email}/{signed_key}>
```
Where `signed_key = isso.sign(["unsubscribe", email])`

### Moderation URLs

**Delete** (isso/ext/notifications.py:122):
```
{public_endpoint}/id/{comment_id}/delete/{signed_key}
```
Where `signed_key = isso.sign(comment_id)`

**Activate** (isso/ext/notifications.py:125):
```
{public_endpoint}/id/{comment_id}/activate/{signed_key}
```

**Unsubscribe** (isso/ext/notifications.py:129-131):
```
{public_endpoint}/id/{parent_id}/unsubscribe/{url_encoded_email}/{signed_key}
```
Where `signed_key = isso.sign(["unsubscribe", email])` with `max_age=2**32`

---

## 9. Tests

**Location**: isso/tests/test_comments.py

**Test functions** (55 tests):

**Creation/Retrieval**:
- `testCreate`: POST /new returns 201, sets cookie
- `testGet`: GET /id/<id> returns comment
- `testCreateMultiple`: Multiple comments get sequential IDs
- `testCreateAndGetMultiple`: Fetch returns all comments
- `testPathVariations`: Different URI encodings work

**Validation**:
- `testVerifyFields`: Text required, min 3 chars, max 65535, parent integer
- `testCreateInvalidParent`: Invalid parent set to root
- `testCreateInvalidThreadForParent`: Cross-thread parent rejected
- `testWebsiteXSSPayloadIsEscaped`: Website quotes escaped

**Editing**:
- `testUpdate`: PUT updates text/author/website, sets new cookie
- `testUpdateForbidden`: Expired cookie blocks edit
- `testUpdateWebsiteXSSPayloadIsEscaped`: Edit also escapes

**Deletion**:
- `testDelete`: DELETE removes comment, deletes cookie
- `testDeleteWithReference`: Referenced comment soft-deleted (mode=4)
- `testDeleteWithMultipleReferences`: Soft-delete with multiple replies
- `testDeleteAndCreateByDifferentUsersButSamePostId`: Cookie not reused
- `testDeleteCommentRemovesThread`: Empty thread deleted

**Voting**:
- Like/dislike: Not explicitly named, but voting tested (MAX_LIKES_AND_DISLIKES=142)

**Fetching/Sorting**:
- `testGetLimited`: limit parameter works
- `testGetLimitedNested`: nested_limit works
- `testGetNested`: Nested comments returned
- `testGetNestedWithOffset`: offset works with nested
- `testGetWithOffset`: offset parameter
- `testGetWithOffsetIgnoredWithoutLimit`: offset ignored if no limit
- `testGetSortedByNewest`: sort=newest DESC
- `testGetSortedByOldest`: sort=oldest ASC
- `testGetSortedByUpvotes`: sort=upvotes (karma DESC)
- `testGetSortedByNewestWithNested`: Sort with nested
- `testGetSortedByUpvotesWithNested`: Sort with nested

**Counts**:
- `testCounts`: POST /count returns array
- `testMultipleCounts`: Multiple URLs counted

**Moderation**:
- `testModerateComment`: Activate pending comment
- `testModerateEditXSSPayloadIsEscaped`: Moderate edit escapes

**Advanced**:
- `testHash`: Hash computation (unit test)
- `testFetch*`: Authorization checks
- `testPreview`: POST /preview renders markdown
- `testFeed`, `testFeedEmpty`, `testNoFeed`: Atom feeds
- `testLatest*`: Latest endpoint
- `testPurge*`: Purge stale comments
- `testUnsubscribe`: Unsubscribe link works
- `testCSRF`: Form-encoded POST rejected
- `testSecureCookie*`: Cookie SameSite values
- `testAddComment`: Direct DB add
- `testTitleNull`: Thread title handling

---

## 10. CLI / Entry Points

**pyproject.toml:39-40**:
```toml
[project.scripts]
isso = "isso:main"
```

**Main entry** (isso/__init__.py, must be run via `isso` command):
- Detects gevent availability and patches if available
- Calls `make_app()` to create WSGI application
- Runs WebServer (by default) or delegates to uWSGI/multiprocessing

**Commands** (inferred from ArgumentParser in `isso/__init__.py`):
- `isso run`: Start server
- Likely supports config path override

(Note: Full argument parser not visible in provided snippets, but the package is invoked as `isso serve` or `isso` with config options.)

---

## Wire Format Summary

**Signing**: itsdangerous.URLSafeTimedSerializer(session_key).dumps(obj)
- Payload: JSON serialized, base64-URL encoded, appended with timestamp and signature
- Verification: max_age in seconds (default 900 for comments, 2^32 for unsubscribe/moderate)

**Hashing**: PBKDF2-SHA1 (1000 iterations, 6-byte output, salt=Eech7co8Ohloopo9Ol6baimi)
- Output: Hex string (12 chars)
- Applied to: email or remote_addr (anonymized)

**IP Anonymization** (isso/utils/__init__.py:16-34):
- IPv4: Zero last octet (192.168.1.1 → 192.168.1.0)
- IPv6: Zero last 5 segments (2001:db8::1 → 2001:db8:::0:0:0:0:0)

**Timestamps**: UNIX epoch (seconds, float with microseconds)

**Text Encoding**: UTF-8

---

## Critical Implementation Details

1. **Comment nesting**: Limited to 1 level (v2→v3 migration flattens deeper nesting)
2. **Bloomfilter**: SHA256-based, 11 hash functions, 256-byte array (not configurable)
3. **Voters re-initialized on upgrade** (v0→v1) due to old signature bug
4. **Text field NOT NULL** (v4→v5 migration)
5. **Session key**: Stored in preferences table, generated once per database
6. **Soft-delete**: Mode 4 clears text/author/website, keeps ID (refs remain valid)
7. **Hard-delete**: Only if no replies, then removed entirely
8. **Cookie token**: [comment_id, sha1(text)] signed with session key
9. **Moderation links**: Signed comment_id only (key=isso.sign(id))
10. **Unsubscribe links**: Signed ["unsubscribe", email] with very long max_age

