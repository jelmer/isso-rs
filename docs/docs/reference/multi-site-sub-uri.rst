Multiple Sites & Sub-URI
========================

.. todo::
   Once Isso has settled on a sensible multi-site configuration which preserves
   full URIs, rework this section.

.. _configure-multiple-sites:

Multiple Sites
--------------

Isso is designed to serve comments for a single website and therefore stores
comments for a relative URL. This is done to support HTTP, HTTPS and even domain transfers
without manual intervention. You can chain Isso to support multiple
websites on different domains.

The following example uses `gunicorn <http://gunicorn.org/>`_ as WSGI server (
you can use uWSGI as well). Let's say you maintain two websites, like
foo.example and other.bar:

.. code-block:: ini
    :emphasize-lines: 3

    ; /etc/isso.d/foo.example.cfg
    [general]
    name = foo
    host = http://foo.example/
    dbpath = /var/lib/isso/foo.example.db

.. code-block:: ini
    :emphasize-lines: 3

    ; /etc/isso.d/other.bar.cfg
    [general]
    name = bar
    host = http://other.bar/
    dbpath = /var/lib/isso/other.bar.db

Then you run ``isso-rs`` with each config passed via its own ``-c`` flag:

.. code-block:: sh

    $ isso-rs -c /etc/isso.d/foo.example.cfg -c /etc/isso.d/other.bar.cfg

In your webserver configuration, proxy Isso as usual:

.. code-block:: nginx

      server {
          listen [::]:80;
          server_name comments.example;

          location / {
              proxy_pass http://localhost:8080;
          }
      }

When you now visit http://comments.example/, you will see your different Isso
configuration separated by ``/name``.

.. code-block:: text

    $ curl http://comments.example/
    /foo
    /bar

Just embed the JavaScript including the new relative path, e.g.
``http://comments.example/foo/js/embed.min.js``. Make sure, you don't mix the
URLs on both sites as it will most likely cause CORS-related errors.

.. note::

   **Multi-site support in Docker**

   The container's ``ENTRYPOINT`` is ``/usr/local/bin/isso-rs -c /config/isso.cfg``,
   so extra ``-c`` flags go after the image name:

   .. code-block:: yaml

      services:
        isso-comments:
          image: isso:latest
          command:
            - "-c"
            - "/config/example1.com.cfg"
            - "-c"
            - "/config/example2.com.cfg"
          # ... other options ...

.. _configure-sub-uri:

Sub-URI
-------

You can run Isso on the same domain as your website, which circumvents issues
originating from CORS_. Also, privacy-protecting browser addons such as
`Request Policy`_ wont block comments.

.. code-block:: nginx
    :emphasize-lines: 9

    server {
        listen       [::]:80;
        listen       [::]:443 ssl;
        server_name  example.tld;
        root         /var/www/example.tld;

        location /isso {
            proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
            proxy_set_header X-Script-Name /isso;
            proxy_set_header Host $host;
            proxy_set_header X-Forwarded-Proto $scheme;
            proxy_pass http://localhost:8080;
        }
    }

.. important::

   When using a sub-URI setup (e.g., serving Isso at ``/isso`` or any other path), you must ensure the Isso client can correctly detect the API endpoint.

   **Recommended approach:** Explicitly set the ``data-isso`` attribute in your embed script to match your sub-URI:

   .. code-block:: html

      <script data-isso="/isso" src="/isso/js/embed.min.js"></script>

   This ensures all API requests are correctly prefixed with your sub-URI (e.g., ``/isso/new``, ``/isso/config``).

   If you omit this attribute, Isso will attempt to auto-detect the endpoint from the script's ``src`` path, which may fail or cause requests to be sent to the root path (``/``) instead of your sub-URI. This can result in broken comment functionality or 404 errors.

Now, the website integration is just as described in
:doc:`/docs/guides/quickstart` but with a different location.

.. _CORS: https://developer.mozilla.org/en/docs/HTTP/Access_control_CORS
.. _Request Policy: https://www.requestpolicy.com/

