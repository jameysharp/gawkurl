# gawkurl: Watch for web page changes

There are many situations where you might want to be informed when a web
page has changed, such as in an RSS reader or calendar app that's
monitoring feeds published elsewhere. But watching a URL for changes is
remarkably complex to do well. This project aims to do that job well, in
order to encourage people to develop more apps that are good internet
citizens.

## What's so difficult about that?

The first thing people generally try is to make their software wake up
periodically, fetch all the pages of interest, and go back to sleep.
This involves a trade-off: if the delay between fetches is too long,
then you don't notice page changes in a timely fashion, but if the delay
is too short, then you may impose a significant burden on the web
server, and they may rate-limit or block your requests. Choosing the
right delay is hard and there isn't a good one-size-fits-all answer.

The next challenge is that many of those page fetches will return the
same content over and over again. A properly configured server should
provide `etag` and/or `last-modified` HTTP headers, and a well-behaved
client should remember those header values and send them back in
`if-none-match` and `if-modified-since` request headers. That gives the
server the opportunity to short-circuit generating the full response and
just send a "304 Not Modified" response instead. But this means the
client needs to correctly remember that state across requests.

Then there's the issue of dealing with temporary network or server
failures. Failed requests should be retried, but this raises a different
set of questions about how quickly to retry, and introduces new
opportunities to anger server administrators.

If the client sends requests too quickly, more sophisticated servers may
return "429 Too Many Requests" with a `retry-after` header indicating
how long the client should wait before trying again. It's often
necessary to respect this signal or risk having further requests
blocked.

Sometimes a single site may have many pages you want to monitor, such as
if many RSS feeds are hosted by the same platform. If you query all the
pages in rapid succession, this can look to smaller servers like an
attack, and again they may rate-limit or block your requests. It's
better to spread requests for a given server out over time.

Finally, some servers support standards such as [WebSub][], which avoids
all of the above problems by simply asking the server to notify you of
changes. However, WebSub only works if you are operating an
always-online web server yourself which can receive those notifications.
Many desktop and mobile apps have no other reason to operate their own
server infrastructure and doing so would make them much more expensive,
so they simply don't use WebSub even if it's available. In turn, many
servers don't offer WebSub because few clients use it.

[WebSub]: https://www.w3.org/TR/websub/

## How can we do better?

I'd like to address each of the above challenges in one project that
focuses on only this one problem domain, and hopefully provide
interfaces to it that are suitable for all different kinds of
applications.

In addition to obvious requirements such as correctly handling headers
like `etag` and `retry-after`, I think there's room for heuristics which
predict when next to poll a page based on the past history of changes to
that page. In many projects it's not worth the effort to experiment with
such heuristics, but if we can do that once in this project and make the
result widely reusable, that's worth doing.

I'd like to make this tool accessible in several different forms:

- server-based with long-polling and CORS headers, enabling some kinds
  of web apps to be deployed as static HTML and JavaScript, making it
  cheap to try new experiments

- server-based with WebSub-like notifications, enabling other kinds of
  web apps to be deployed on Function-as-a-Service platforms or other
  environments where they can only do work in response to an HTTP
  request

- server-based with [Push API][] notifications, enabling Progressive Web
  Apps that don't use any CPU or battery life while waiting for changes

- embedded in-process as a library, enabling native apps that don't
  depend on a server

[Push API]: https://www.w3.org/TR/push-api/

If many people share the same server-based deployment and request the
same pages, our server only has to request those pages once. That
reduces the load on the origin servers, further avoiding burdening them.
