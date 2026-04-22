Deployment
----------

``isso`` is a standalone HTTP server — no WSGI, no gunicorn, no uWSGI.
It binds either a TCP port or a Unix socket directly and serves requests
on a tokio runtime. For production you put a reverse proxy (nginx, caddy,
apache) in front of it.

.. contents::
    :local:
    :depth: 1


Standalone (TCP)
^^^^^^^^^^^^^^^^

Set ``[server] listen`` to the host:port you want ``isso`` to bind to:

.. code-block:: ini

    [server]
    listen = http://127.0.0.1:8080

Then start the server:

.. code-block:: sh

    $ isso -c /etc/isso.cfg

See :ref:`init-scripts` for a systemd unit to run it as a service.


Unix socket (behind a reverse proxy)
^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

For deployments behind nginx/caddy, use a Unix socket — it's faster than
loopback TCP and easier to lock down with filesystem permissions. Set:

.. code-block:: ini

    [server]
    listen = unix:///run/isso.sock

``isso`` will create (and replace any stale) socket at that path on
startup. Point your reverse proxy at it:

.. code-block:: nginx

    upstream isso {
        server unix:/run/isso.sock;
    }

    server {
        server_name comments.example.com;
        listen 443 ssl;

        location / {
            proxy_pass http://isso;
            proxy_set_header Host              $host;
            proxy_set_header X-Real-IP         $remote_addr;
            proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;
            proxy_set_header X-Forwarded-Proto $scheme;
            proxy_set_header X-Forwarded-Host  $host;
        }
    }

Set ``[server] trusted-proxies`` to the IP(s) of the reverse proxy so
``isso`` will honour ``X-Forwarded-For`` to recover the real client IP
(otherwise every comment gets logged from the proxy's IP — see :ref:`xff`).


.. _xff:

X-Forwarded-For and trusted-proxies
^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

By default, ``isso`` **ignores** ``X-Forwarded-For`` and uses the direct
TCP peer as the commenter's IP. That's the safe default — any client could
send their own XFF header, so trusting it without a whitelist would let
them spoof their /24.

To opt in, list the IPs of your reverse proxy layer under
``[server] trusted-proxies``:

.. code-block:: ini

    [server]
    trusted-proxies =
      127.0.0.1
      10.0.0.5

With that set, ``isso`` walks the ``X-Forwarded-For`` chain right-to-left,
skipping entries that appear in the trusted-proxies list, and takes the
first untrusted hop as the client. Falls back to the TCP peer if the whole
chain is trusted.


Sub-path (reverse proxy under /isso/)
^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

If your reverse proxy mounts ``isso`` at a sub-path (e.g.
``https://example.com/isso/``), forward ``X-Forwarded-Prefix`` and
``X-Forwarded-Host``:

.. code-block:: nginx

    location /isso/ {
        proxy_pass http://isso/;
        proxy_set_header X-Forwarded-Host   $host;
        proxy_set_header X-Forwarded-Prefix /isso;
        proxy_set_header X-Forwarded-Proto  $scheme;
    }

``isso`` uses those headers to reconstruct the external URL for
admin-UI links and moderation emails. You can also hard-code the
external URL with ``[server] public-endpoint`` — whichever is set takes
precedence.


Multi-site
^^^^^^^^^^

Pass ``-c`` multiple times to serve multiple sites from a single process.
Each config's ``[general] name`` becomes a sub-path the site mounts under:

.. code-block:: sh

    $ isso -c site-a.cfg -c site-b.cfg

With ``name = alpha`` in ``site-a.cfg`` and ``name = beta`` in
``site-b.cfg``, requests to ``/alpha/...`` go to site A and ``/beta/...``
to site B. The listen address comes from the first config.
