// ------------------------------------------------------------------------------------------
// History.
// ------------------------------------------------------------------------------------------

/**
* @api {get} /demo Isso demo page
* @apiGroup Demo
* @apiName demo
* @apiVersion 0.12.6
* @apiPrivate
* @apiDescription
*      Displays a demonstration of Isso with a thread counter and comment widget.
*
* @apiExample {curl} Get demo page
*     curl 'https://comments.example.com/demo/index.html'
*
* @apiSuccessExample {html} Demo page:
*     <!DOCTYPE html>
*     <head>
*      <title>Isso Demo</title>
*      <meta charset="utf-8">
*      <meta name="viewport" content="width=device-width, initial-scale=1">
*     </head>
*     <body>
*      <div id="page">
*       <div id="wrapper" style="max-width: 900px; margin-left: auto; margin-right: auto;">
*        <h2><a href="index.html">Isso Demo</a></h2>
*        <script src="../js/embed.dev.js" data-isso="../" ></script>
*        <section>
*          <p>This is a link to a thead, which will display a comment counter:
*          <a href="/demo/index.html#isso-thread">How many Comments?</a></p>
*          <p>Below is the actual comment field.</p>
*        </section>
*        <section id="isso-thread" data-title="Isso Test"><noscript>Javascript needs to be activated to view comments.</noscript></section>
*       </div>
*      </div>
*     </body>
*/

/**
* @api {get} /count (Deprecated) Count for single thread
* @apiGroup Thread
* @apiName count
* @apiVersion 0.12.6
* @apiDeprecated use (#Thread:counts) instead.
* @apiDescription
*     (Deprecated) Counts the number of comments for a single thread.

* @apiBody {Number[]} urls
*     Array of URLs for which to fetch comment counts

* @apiExample {curl} Get the respective counts of 5 threads:
*     curl 'https://comments.example.com/count' -d '["/blog/firstPost.html", "/blog/controversalPost.html", "/blog/howToCode.html", "/blog/boringPost.html", "/blog/isso.html"]

* @apiSuccessExample {json} Counts of 5 threads:
*     [2, 18, 4, 0, 3]
*/

/**
* @api {post} /new create new
* @apiGroup Comment
* @apiName new
* @apiVersion 0.12.6
* @apiDescription
*     Creates a new comment. The server issues a cookie per new comment which acts as
*     an authentication token to modify or delete the comment.
*     The token is cryptographically signed and expires automatically after 900 seconds (=15min) by default.
* @apiUse csrf

* @apiQuery {String} uri
*     The uri of the thread to create the comment on.
* @apiBody {String{3..}} text
*     The comment’s raw text.
* @apiBody {String} [author]
*     The comment’s author’s name.
* @apiBody {String} [email]
*     The comment’s author’s email address.
* @apiBody {String} [website]
*     The comment’s author’s website’s url.
* @apiBody {number} [parent]
*     The parent comment’s id if the new comment is a response to an existing comment.

* @apiExample {curl} Create a reply to comment with id 15:
*     curl 'https://comments.example.com/new?uri=/thread/' -d '{"text": "Stop saying that! *isso*!", "author": "Max Rant", "email": "rant@example.com", "parent": 15}' -H 'Content-Type: application/json' -c cookie.txt

* @apiUse commentResponse

* @apiSuccessExample {json} Success after the above request:
*     HTTP/1.1 201 CREATED
*     Set-Cookie: 1=...; Expires=Wed, 18-Dec-2013 12:57:20 GMT; Max-Age=900; Path=/
*     X-Set-Cookie: isso-1=...; Expires=Wed, 18-Dec-2013 12:57:20 GMT; Max-Age=900; Path=/
*     {
*         "website": null,
*         "author": "Max Rant",
*         "parent": 15,
*         "created": 1464940838.254393,
*         "text": "&lt;p&gt;Stop saying that! &lt;em&gt;isso&lt;/em&gt;!&lt;/p&gt;",
*         "dislikes": 0,
*         "modified": null,
*         "mode": 1,
*         "hash": "e644f6ee43c0",
*         "id": 23,
*         "likes": 0
*     }
*/

// ------------------------------------------------------------------------------------------
// The blocks below were originally docstrings inside isso/views/comments.py in the
// Python implementation. After the Rust port we keep them here as a standalone
// JavaScript file so the apidoc generator has a single input path.
// ------------------------------------------------------------------------------------------

/**
*
*     @apiDefine csrf
*     @apiHeader {String="application/json"} Content-Type
*         The content type must be set to `application/json` to prevent CSRF attacks.
*/

/**
*
*     @apiDefine plainParam
*     @apiQuery {Number=0,1} [plain=0]
*         If set to `1`, the plain text entered by the user will be returned in the comments’ `text` attribute (instead of the rendered markdown).
*/

/**
*
*     @apiDefine commentResponse
*
*     @apiSuccess {Number} id
*         The comment’s id (assigned by the server).
*     @apiSuccess {Number} parent
*         Id of the comment this comment is a reply to. `null` if this is a top-level-comment.
*     @apiSuccess {Number=1,2,4} mode
*         The comment’s mode:
*         value | explanation
*          ---  | ---
*          `1`  | accepted: The comment was accepted by the server and is published.
*          `2`  | in moderation queue: The comment was accepted by the server but awaits moderation.
*          `4`  | deleted, but referenced: The comment was deleted on the server but is still referenced by replies.
*     @apiSuccess {String} author
*         The comments’s author’s name or `null`.
*     @apiSuccess {String} website
*         The comment’s author’s website or `null`.
*     @apiSuccess {String} hash
*         A hash uniquely identifying the comment’s author.
*     @apiSuccess {Number} created
*         UNIX timestamp of the time the comment was created (on the server).
*     @apiSuccess {Number} modified
*         UNIX timestamp of the time the comment was last modified (on the server). `null` if the comment was not yet modified.
*/

/**
*
*     @apiDefine admin Admin access needed
*         Only available to a logged-in site admin. Requires a valid `admin-session` cookie.
*/

/**
*
*     @api {post} /new create new
*     @apiGroup Comment
*     @apiName new
*     @apiVersion 0.12.6
*     @apiDescription
*         Creates a new comment. The server issues a cookie per new comment which acts as
*         an authentication token to modify or delete the comment.
*         The token is cryptographically signed and expires automatically after 900 seconds (=15min) by default.
*     @apiUse csrf
*
*     @apiQuery {String} uri
*         The uri of the thread to create the comment on.
*     @apiBody {String{3...65535}} text
*         The comment’s raw text.
*     @apiBody {String} [author]
*         The comment’s author’s name.
*     @apiBody {String{...254}} [email]
*         The comment’s author’s email address.
*     @apiBody {String{...254}} [website]
*         The comment’s author’s website’s url. Must be Django-conform, i.e. either `http(s)://example.com/foo` or `example.com/`
*     @apiBody {Number} [parent]
*         The parent comment’s id if the new comment is a response to an existing comment.
*     @apiBody {String} [title]
*         The title of the thread. Required when creating the first comment for a new thread if the title cannot be automatically fetched from the URI.
*
*     @apiExample {curl} Create a reply to comment with id 15:
*         curl 'https://comments.example.com/new?uri=/thread/' -d '{"text": "Stop saying that! *isso*!", "author": "Max Rant", "email": "rant@example.com", "parent": 15}' -H 'Content-Type: application/json' -c cookie.txt
*
*     @apiUse commentResponse
*
*     @apiSuccessExample {json} Success after the above request:
*         HTTP/1.1 201 CREATED
*         Set-Cookie: 1=...; Expires=Wed, 18-Dec-2013 12:57:20 GMT; Max-Age=900; Path=/; SameSite=Lax
*         X-Set-Cookie: isso-1=...; Expires=Wed, 18-Dec-2013 12:57:20 GMT; Max-Age=900; Path=/; SameSite=Lax
*         {
*             "website": null,
*             "author": "Max Rant",
*             "parent": 15,
*             "created": 1464940838.254393,
*             "text": "&lt;p&gt;Stop saying that! &lt;em&gt;isso&lt;/em&gt;!&lt;/p&gt;",
*             "dislikes": 0,
*             "modified": null,
*             "mode": 1,
*             "hash": "e644f6ee43c0",
*             "id": 23,
*             "likes": 0
*         }
*/

/**
*
*     @api {get} /id/:id view
*     @apiGroup Comment
*     @apiName view
*     @apiVersion 0.12.6
*     @apiDescription
*         View an existing comment, for the purpose of editing. Editing a comment is only possible for a short period of time (15min by default) after it was created and only if the requestor has a valid cookie for it. See the [Isso server documentation](https://isso-comments.de/docs/reference/server-config/) for details.
*
*     @apiParam {Number} id
*         The id of the comment to view.
*     @apiUse plainParam
*
*     @apiExample {curl} View the comment with id 4:
*         curl 'https://comments.example.com/id/4' -b cookie.txt
*
*     @apiUse commentResponse
*
*     @apiSuccessExample Example result:
*         {
*             "website": null,
*             "author": null,
*             "parent": null,
*             "created": 1464914341.312426,
*             "text": " &lt;p&gt;I want to use MySQL&lt;/p&gt;",
*             "dislikes": 0,
*             "modified": null,
*             "mode": 1,
*             "id": 4,
*             "likes": 1
*         }
*/

/**
*
*     @api {put} /id/:id edit
*     @apiGroup Comment
*     @apiName edit
*     @apiVersion 0.12.6
*     @apiDescription
*         Edit an existing comment. Editing a comment is only possible for a short period of time (15min by default) after it was created and only if the requestor has a valid cookie for it. See the [Isso server documentation](https://isso-comments.de/docs/reference/server-config/) for details. Editing a comment will set a new edit cookie in the response.
*     @apiUse csrf
*
*     @apiParam {Number} id
*         The id of the comment to edit.
*     @apiBody {String{3...65535}} text
*         A new (raw) text for the comment.
*     @apiBody {String} [author]
*         The modified comment’s author’s name.
*     @apiBody {String{...254}} [website]
*         The modified comment’s author’s website. Must be Django-conform, i.e. either `http(s)://example.com/foo` or `example.com/`
*
*     @apiExample {curl} Edit comment with id 23:
*         curl -X PUT 'https://comments.example.com/id/23' -d {"text": "I see your point. However, I still disagree.", "website": "maxrant.important.com"} -H 'Content-Type: application/json' -b cookie.txt
*
*     @apiUse commentResponse
*
*     @apiSuccessExample {json} Example response:
*         HTTP/1.1 200 OK
*         {
*             "website": "maxrant.important.com",
*             "author": "Max Rant",
*             "parent": 15,
*             "created": 1464940838.254393,
*             "text": "&lt;p&gt;I see your point. However, I still disagree.&lt;/p&gt;",
*             "dislikes": 0,
*             "modified": 1464943439.073961,
*             "mode": 1,
*             "id": 23,
*             "likes": 0
*         }
*/

/**
*
*     @api {delete} /id/:id delete
*     @apiGroup Comment
*     @apiName delete
*     @apiVersion 0.12.6
*     @apiDescription
*         Delete an existing comment. Deleting a comment is only possible for a short period of time (15min by default) after it was created and only if the requestor has a valid cookie for it. See the [Isso server documentation](https://isso-comments.de/docs/reference/server-config/) for details.
*         Returns either `null` or a comment with an empty text value when the comment is still referenced by other comments.
*     @apiUse csrf
*
*     @apiParam {Number} id
*         Id of the comment to delete.
*
*     @apiExample {curl} Delete comment with id 14:
*         curl -X DELETE 'https://comments.example.com/id/14' -b cookie.txt
*
*     @apiSuccessExample Successful deletion returns null and deletes cookie:
*         HTTP/1.1 200 OK
*         Set-Cookie 14=; Expires=Thu, 01 Jan 1970 00:00:00 GMT; Max-Age=0; Path=/; SameSite=Lax
*         X-Set-Cookie 14=; Expires=Thu, 01 Jan 1970 00:00:00 GMT; Max-Age=0; Path=/; SameSite=Lax
*
*         null
*
*     @apiSuccessExample {json} Comment still referenced by another:
*         HTTP/1.1 200 OK
*         Set-Cookie 14=; Expires=Thu, 01 Jan 1970 00:00:00 GMT; Max-Age=0; Path=/; SameSite=Lax
*         X-Set-Cookie 14=; Expires=Thu, 01 Jan 1970 00:00:00 GMT; Max-Age=0; Path=/; SameSite=Lax
*         {
*             "id": 14,
*             "parent": null,
*             "created": 1653432621.0512516,
*             "modified": 1653434488.571937,
*             "mode": 4,
*             "text": "",
*             "author": null,
*             "website": null,
*             "likes": 0,
*             "dislikes": 0,
*             "notification": 0
*         }
*/

/**
*
*     @api {get} /id/:id/unsubscribe/:email/:key unsubscribe
*     @apiGroup Comment
*     @apiName unsubscribe
*     @apiVersion 0.12.6
*     @apiDescription
*         Opt out from getting any further email notifications about replies to a particular comment. In order to use this endpoint, the requestor needs a `key` that is usually obtained from an email sent out by isso.
*
*     @apiParam {Number} id
*         The id of the comment to unsubscribe from replies to.
*     @apiParam {String} email
*         The email address of the subscriber.
*     @apiParam {String} key
*         The key to authenticate the subscriber.
*
*     @apiExample {curl} Unsubscribe Alice from replies to comment with id 13:
*         curl -X GET 'https://comments.example.com/id/13/unsubscribe/alice@example.com/WyJ1bnN1YnNjcmliZSIsImFsaWNlQGV4YW1wbGUuY29tIl0.DdcH9w.Wxou-l22ySLFkKUs7RUHnoM8Kos'
*
*     @apiSuccessExample {html} Using GET:
*         <!DOCTYPE html>
*         <html>
*             <head&gtSuccessfully unsubscribed</head>
*             <body>
*               <p>You have been unsubscribed from replies in the given conversation.</p>
*             </body>
*         </html>
*/

/**
*
*     @api {post} /id/:id/:action/:key moderate
*     @apiGroup Comment
*     @apiName moderate
*     @apiVersion 0.12.6
*     @apiDescription
*         Publish or delete a comment that is in the moderation queue (mode `2`). In order to use this endpoint, the requestor needs a `key` that is usually obtained from an email sent out by Isso or provided in the admin interface.
*         This endpoint can also be used with a `GET` request. In that case, a html page is returned that asks the user whether they are sure to perform the selected action. If they select “yes”, the query is repeated using `POST`.
*
*     @apiParam {Number} id
*         The id of the comment to moderate.
*     @apiParam {String=activate,edit,delete} action
*         - `activate` to publish the comment (change its mode to `1`).
*         - `edit`: Send `text`, `author`, `email` and `website` via `POST`.
*            To be used from the admin interface. Better use the `edit` `PUT` endpoint.
*         - `delete` to delete the comment.
*     @apiParam {String} key
*         The moderation key to authenticate the moderation.
*
*     @apiExample {curl} delete comment with id 13:
*         curl -X POST 'https://comments.example.com/id/13/delete/MTM.CjL6Fg.REIdVXa-whJS_x8ojQL4RrXnuF4'
*
*     @apiSuccessExample {html} Request deletion using GET:
*         <!DOCTYPE html>
*         <html>
*             <head>
*                 <script>
*                     if (confirm('Delete: Are you sure?')) {
*                         xhr = new XMLHttpRequest;
*                         xhr.open('POST', window.location.href);
*                         xhr.send(null);
*                         xhr.onload = function() {
*                             window.location.href = "https://example.com/example-thread/#isso-13";
*                         };
*                     }
*                 </script>
*
*     @apiSuccessExample Delete using POST:
*         Comment has been deleted
*
*     @apiSuccessExample Activate using POST:
*         Comment has been activated
*/

/**
*
*     @api {get} / Get comments
*     @apiGroup Thread
*     @apiName fetch
*     @apiVersion 0.13.1
*     @apiDescription Queries the publicly visible comments of a thread.
*
*     @apiQuery {String} uri
*         The URI of thread to get the comments from.
*     @apiQuery {Number} [parent]
*         Return only comments that are children of the comment with the provided ID.
*     @apiUse plainParam
*     @apiQuery {Number} [limit]
*         The maximum number of returned top-level comments. Omit for unlimited results.
*     @apiQuery {Number} [nested_limit]
*         The maximum number of returned nested comments per comment. Omit for unlimited results.
*     @apiQuery {Number} [after]
*         Includes only comments were added after the provided UNIX timestamp.
*     @apiQuery {String} [sort]
*         The sorting order of the comments. Possible values are `newest`, `oldest`, `upvotes`. If omitted, default sort order will be `oldest`.
*     @apiQuery {Number} [offset]
*         Offset the returned comments by this number. Used for pagination. Works only in combination with `limit`.
*
*     @apiSuccess {Number} id
*         Id of the comment `replies` is the list of replies of. `null` for the list of top-level comments.
*     @apiSuccess {Number} total_replies
*         The number of replies if the `limit` parameter was not set. If `after` is set to `X`, this is the number of comments that were created after `X`. So setting `after` may change this value!
*     @apiSuccess {Number} hidden_replies
*         The number of comments that were omitted from the results because of the `limit` request parameter. Usually, this will be `total_replies` - `limit`.
*     @apiSuccess {Object[]} replies
*         The list of comments. Each comment also has the `total_replies`, `replies`, `id` and `hidden_replies` properties to represent nested comments.
*     @apiSuccess {Object[]} config
*         Object holding only the client configuration parameters that depend on server settings. Will be dropped in a future version of Isso. Use the dedicated `/config` endpoint instead.
*
*     @apiExample {curl} Get 2 comments with 5 responses:
*         curl 'https://comments.example.com/?uri=/thread/&limit=2&nested_limit=5'
*     @apiSuccessExample {json} Example response:
*         {
*           "total_replies": 14,
*           "replies": [
*             {
*               "website": null,
*               "author": null,
*               "parent": null,
*               "created": 1464818460.732863,
*               "text": "&lt;p&gt;Hello, World!&lt;/p&gt;",
*               "total_replies": 1,
*               "hidden_replies": 0,
*               "dislikes": 2,
*               "modified": null,
*               "mode": 1,
*               "replies": [
*                 {
*                   "website": null,
*                   "author": null,
*                   "parent": 1,
*                   "created": 1464818460.769638,
*                   "text": "&lt;p&gt;Hi, now some Markdown: &lt;em&gt;Italic&lt;/em&gt;, &lt;strong&gt;bold&lt;/strong&gt;, &lt;code&gt;monospace&lt;/code&gt;.&lt;/p&gt;",
*                   "dislikes": 0,
*                   "modified": null,
*                   "mode": 1,
*                   "hash": "2af4e1a6c96a",
*                   "id": 2,
*                   "likes": 2
*                 }
*               ],
*               "hash": "1cb6cc0309a2",
*               "id": 1,
*               "likes": 2
*             },
*             {
*               "website": null,
*               "author": null,
*               "parent": null,
*               "created": 1464818460.80574,
*               "text": "&lt;p&gt;Lorem ipsum dolor sit amet, consectetur adipisicing elit. Accusantium at commodi cum deserunt dolore, error fugiat harum incidunt, ipsa ipsum mollitia nam provident rerum sapiente suscipit tempora vitae? Est, qui?&lt;/p&gt;",
*               "total_replies": 0,
*               "hidden_replies": 0,
*               "dislikes": 0,
*               "modified": null,
*               "mode": 1,
*               "replies": [],
*               "hash": "1cb6cc0309a2",
*               "id": 3,
*               "likes": 0
*             },
*             "id": null,
*             "hidden_replies": 12
*         }
*/

/**
*
*     @apiDefine likeResponse
*     @apiSuccess {Number} likes
*         The (new) number of likes on the comment.
*     @apiSuccess {Number} dislikes
*         The (new) number of dislikes on the comment.
*     @apiSuccessExample Return updated vote counts:
*         {
*             "likes": 4,
*             "dislikes": 3
*         }
*/

/**
*
*     @api {post} /id/:id/like like
*     @apiGroup Comment
*     @apiName like
*     @apiVersion 0.12.6
*     @apiDescription
*          Puts a “like” on a comment. The author of a comment cannot like their own comment.
*     @apiUse csrf
*
*     @apiParam {Number} id
*         The id of the comment to like.
*
*     @apiExample {curl} Like comment with id 23:
*         curl -X POST 'https://comments.example.com/id/23/like'
*
*     @apiUse likeResponse
*/

/**
*
*     @api {post} /id/:id/dislike dislike
*     @apiGroup Comment
*     @apiName dislike
*     @apiVersion 0.12.6
*     @apiDescription
*          Puts a “dislike” on a comment. The author of a comment cannot dislike their own comment.
*     @apiUse csrf
*
*     @apiParam {Number} id
*         The id of the comment to dislike.
*
*     @apiExample {curl} Dislike comment with id 23:
*         curl -X POST 'https://comments.example.com/id/23/dislike'
*
*     @apiUse likeResponse
*/

/**
*
*     @api {post} /preview preview
*     @apiGroup Comment
*     @apiName preview
*     @apiVersion 0.12.6
*     @apiDescription
*         Render comment text using markdown.
*
*     @apiBody {String{3...65535}} text
*         (Raw) comment text
*
*     @apiSuccess {String} text
*         Rendered comment text
*
*     @apiExample {curl} Preview comment:
*         curl -X POST 'https://comments.example.com/preview' -d '{"text": "A sample comment"}'
*
*     @apiSuccessExample {json} Rendered comment:
*         {
*             "text": "<p>A sample comment</p>"
*         }
*/

/**
*
*     @api {post} /count Count comments
*     @apiGroup Thread
*     @apiName counts
*     @apiVersion 0.12.6
*     @apiDescription
*         Counts the number of comments on multiple threads. The requestor provides a list of thread uris. The number of comments on each thread is returned as a list, in the same order as the threads were requested. The counts include comments that are responses to comments, but only published comments (i.e. exclusing comments pending moderation).
*
*     @apiBody {Number[]} urls
*         Array of URLs for which to fetch comment counts
*
*     @apiExample {curl} Get the respective counts of 5 threads:
*         curl -X POST 'https://comments.example.com/count' -d '["/blog/firstPost.html", "/blog/controversalPost.html", "/blog/howToCode.html", "/blog/boringPost.html", "/blog/isso.html"]
*
*     @apiSuccessExample {json} Counts of 5 threads:
*         [2, 18, 4, 0, 3]
*/

/**
*
*     @api {get} /feed Atom feed for comments
*     @apiGroup Thread
*     @apiName feed
*     @apiVersion 0.12.6
*     @apiDescription
*         Provide an Atom feed for the given thread. Only available if `[rss] base` is set in server config. By default, up to 100 comments are returned.
*
*     @apiQuery {String} uri
*         The uri of the thread to display a feed for
*
*     @apiExample {curl} Get an Atom feed for /thread/foo in XML format:
*         curl 'https://comments.example.com/feed?uri=/thread/foo'
*
*     @apiSuccessExample Atom feed for /thread/foo:
*         <?xml version='1.0' encoding='utf-8'?>
*         <feed xmlns="http://www.w3.org/2005/Atom" xmlns:thr="http://purl.org/syndication/thread/1.0">
*           <updated>2022-05-24T20:38:04.032789Z</updated>
*           <id>tag:example.com,2018:/isso/thread/thread/foo</id>
*           <title>Comments for example.com/thread/foo</title>
*           <entry>
*             <id>tag:example.com,2018:/isso/1/2</id>
*             <title>Comment #2</title>
*             <updated>2022-05-24T20:38:04.032789Z</updated>
*             <author>
*               <name>John Doe</name>
*             </author>
*             <link href="http://example.com/thread/foo#isso-2" />
*             <content type="html">&lt;p&gt;And another&lt;/p&gt;</content>
*           </entry>
*           <entry>
*             <id>tag:example.com,2018:/isso/1/1</id>
*             <title>Comment #1</title>
*             <updated>2022-05-24T20:38:00.837703Z</updated>
*             <author>
*               <name>Jane Doe</name>
*             </author>
*             <link href="http://example.com/thread/foo#isso-1" />
*             <content type="html">&lt;p&gt;A sample comment&lt;/p&gt;</content>
*           </entry>
*         </feed>
*/

/**
*
*     @api {get} /config Fetch client config
*     @apiGroup Thread
*     @apiName config
*     @apiVersion 0.13.0
*     @apiDescription
*         Returns only the client configuration parameters that depend on server settings.
*
*     @apiSuccess {Object[]} config
*         The client configuration object.
*     @apiSuccess {Boolean} config.reply-to-self
*         Commenters can reply to their own comments.
*     @apiSuccess {Boolean} config.require-author
*         Commenters must enter valid Name.
*     @apiSuccess {Boolean} config.require-email
*         Commenters must enter valid email.
*     @apiSuccess {Boolean} config.reply-notifications
*         Enable reply notifications via E-mail.
*     @apiSuccess {Boolean} config.gravatar
*         Load images from Gravatar service instead of generating them. Also disables regular avatars (see below).
*     @apiSuccess {Boolean} config.avatar
*         To avoid having both regular avatars and Gravatars side-by-side,
*         setting `gravatar` will disable regular avatars. The `avatar` key will
*         only be sent by the server if `gravatar` is set.
*     @apiSuccess {Boolean} config.feed
*         Enable or disable the addition of a link to the feed for the comment
*         thread.
*
*     @apiExample {curl} get the client config:
*         curl 'https://comments.example.com/config'
*
*     @apiSuccessExample {json} Client config:
*         {
*           "config": {
*             "reply-to-self": false,
*             "require-email": false,
*             "require-author": false,
*             "reply-notifications": false,
*             "gravatar": true,
*             "avatar": false,
*             "feed": false
*           }
*         }
*/

/**
*
*     @api {get} /demo/ Isso demo page
*     @apiGroup Demo
*     @apiName demo
*     @apiVersion 0.13.0
*     @apiPrivate
*     @apiDescription
*          Displays a demonstration of Isso with a thread counter and comment widget.
*
*     @apiExample {curl} Get demo page
*         curl 'https://comments.example.com/demo/'
*
*     @apiSuccessExample {html} Demo page:
*         <!DOCTYPE html>
*         <head>
*          <title>Isso Demo</title>
*          <meta charset="utf-8">
*          <meta name="viewport" content="width=device-width, initial-scale=1">
*         </head>
*         <body>
*          <div id="page">
*           <div id="wrapper" style="max-width: 900px; margin-left: auto; margin-right: auto;">
*            <h2><a href=".">Isso Demo</a></h2>
*            <script src="../js/embed.dev.js" data-isso="../" ></script>
*            <div>
*              <p>This is a link to a thead, which will display a comment counter:
*              <a href=".#isso-thread">How many Comments?</a></p>
*              <p>Below is the actual comment field.</p>
*            </div>
*            <div id="isso-thread" data-title="Isso Test"><noscript>Javascript needs to be activated to view comments.</noscript></div>
*           </div>
*          </div>
*         </body>
*/

/**
*
*     @api {post} /login/ Log in
*     @apiGroup Admin
*     @apiName login
*     @apiVersion 0.12.6
*     @apiPrivate
*     @apiDescription
*          Log in to admin, will redirect to `/admin/` on success. Must use form data, not `POST` JSON.
*
*     @apiBody {String} password
*         The admin password as set in `[admin] password` in the server config.
*
*     @apiExample {curl} Log in
*         curl -X POST 'https://comments.example.com/login' -F "password=strong_default_password_for_isso_admin" -c cookie.txt
*
*     @apiSuccessExample {html} Login successful:
*         <!doctype html>
*         <html lang=en>
*         <title>Redirecting...</title>
*         <h1>Redirecting...</h1>
*         <p>You should be redirected automatically to the target URL: <a href="https://comments.example.com/admin/">https://comments.example.com/admin/</a>. If not, click the link.
*/

/**
*
*     @api {get} /admin/ Admin interface
*     @apiGroup Admin
*     @apiName admin
*     @apiVersion 0.12.6
*     @apiPrivate
*     @apiPermission admin
*     @apiDescription
*          Display an admin interface from which to manage comments. Will redirect to `/login` if not already logged in.
*
*     @apiQuery {Number} [page=0]
*         Page number
*     @apiQuery {Number{1,2,4}} [mode=2]
*         The comment’s mode:
*         value | explanation
*          ---  | ---
*          `1`  | accepted: The comment was accepted by the server and is published.
*          `2`  | in moderation queue: The comment was accepted by the server but awaits moderation.
*          `4`  | deleted, but referenced: The comment was deleted on the server but is still referenced by replies.
*     @apiQuery {String{id,created,modified,likes,dislikes,tid}} [order_by=created]
*         Comment ordering
*     @apiQuery {Number{0,1}} [asc=0]
*         Ascending
*     @apiQuery {String} comment_search_url
*         Search comments by URL. Both threads and individual comments are valid.
*         For example, a thread might have a URL like 'http://example.com/thread'
*         and an individual comment might have a URL like 'http://example.com/thread#isso-1'
*
*     @apiExample {curl} Listing of published comments:
*         curl 'https://comments.example.com/admin/?mode=1&page=0&order_by=modified&asc=1' -b cookie.txt
*/

/**
*
*     @api {get} /latest latest
*     @apiGroup Comment
*     @apiName latest
*     @apiVersion 0.12.6
*     @apiDescription
*         Get the latest comments from the system, no matter which thread. Only available if `[general] latest-enabled` is set to `true` in server config.
*
*     @apiQuery {Number} limit
*         The quantity of last comments to retrieve
*
*     @apiExample {curl} Get the latest 5 comments
*         curl 'https://comments.example.com/latest?limit=5'
*
*     @apiUse commentResponse
*
*     @apiSuccessExample Example result:
*         [
*             {
*                 "website": null,
*                 "uri": "/some",
*                 "author": null,
*                 "parent": null,
*                 "created": 1464912312.123416,
*                 "text": " &lt;p&gt;I want to use MySQL&lt;/p&gt;",
*                 "dislikes": 0,
*                 "modified": null,
*                 "mode": 1,
*                 "id": 3,
*                 "likes": 1
*             },
*             {
*                 "website": null,
*                 "uri": "/other",
*                 "author": null,
*                 "parent": null,
*                 "created": 1464914341.312426,
*                 "text": " &lt;p&gt;I want to use MySQL&lt;/p&gt;",
*                 "dislikes": 0,
*                 "modified": null,
*                 "mode": 1,
*                 "id": 4,
*                 "likes": 0
*             }
*         ]
*/
