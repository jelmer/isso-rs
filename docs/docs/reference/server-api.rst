Server API
==========

.. note:: View the `Current API documentation`_ for **Isso 0.12.6** here, which
   is automatically generated. You can select previous versions from a dropdown
   on the upper right of the page.

    Using the API, you can:

   - Fetch comment threads
   - Post, edit and delete comments
   - Get information about the server
   - Like and dislike comments
   - **...and much more!**

The Isso API uses ``HTTP`` and ``JSON`` as primary communication protocol.
The API is extensively documented using an `apiDoc`_-compatible syntax in
`apidoc/_apidoc.js`_ — the original upstream annotations used to live
inline as docstrings in ``isso/views/comments.py``; after the Rust port
they were consolidated into a single JavaScript file so the apidoc
pipeline has a single input tree.

.. _Current API documentation: /docs/api/
.. _apiDoc: https://apidocjs.com/
.. _apidoc/_apidoc.js: https://github.com/isso-comments/isso/blob/rust/apidoc/_apidoc.js

Sections covered in this document:

.. contents::
    :local:

Generating API documentation
----------------------------

Install ``Node.js`` and ``npm``.

Run ``make apidoc-init apidoc`` and view the generated API documentation at
``apidoc/_output/`` (it produces a regular HTML file).

Live API testing
----------------

To test out calls to the API right from the browser, without having to
copy-&-paste ``curl`` commands, you can use ``apiDoc``'s live preview
functionality.

Set ``sampleUrl`` to e.g. ``localhost:8080`` in ``apidoc.json``:

.. code-block:: json
   :caption: apidoc.json

    {
      "name": "Isso API",
      "version": "0.13.0",
      "sampleUrl": "http://localhost:8080",
      "private": "true"
    }

Run ``make apidoc`` again and start your local
:ref:`test server <development-server>`

Go to ``apidoc/output`` and serve the generated API docs via
``python -m http.server`` [#f1]_, open ``http://localhost:8000`` in your browser
and use the "Send a sample request"

.. image:: /images/apidoc-sample-latest.png
   :scale: 75 %

.. [#f1] You must use a webserver to view the docs. Opening the local file
   straight from the browser will not work; the browser will refuse to execute
   any ``GET``/``POST`` calls because of security issues with local files.

Writing API documentation
-------------------------

Isso's API documentation is built using the `apiDoc`_ Javascript tool.

Inside `apidoc/_apidoc.js`_, each public endpoint has a JavaScript block
comment annotated using ``@api`` syntax. The source Rust handlers live
in ``src/server/handlers.rs`` — when adding or renaming an
endpoint, add the corresponding ``@api`` block to ``apidoc/_apidoc.js``
by hand (the apidoc pipeline doesn't scrape Rust sources directly).

.. note:: The `apiDoc`_ "Getting started" guide should also help you get up to
   speed in making the API documentation of Isso even better!

A few points to consider:

- Use ``@apiVersion`` to annotate when an endpoint was first introduced or
  changed. This information will help to automatically create a viewable diff
  between Isso API versions.
- The current documentation for all endpoints should be good enough to
  copy-paste for your new or changed endpoint.
- Admin functionality is marked ``@apiPrivate``. To generate docs for private
  endpoints, set ``--private`` on the ``apidoc`` command line.
- Use ``@apiQuery`` for GET query URL-encoded parameters, ``@apiBody`` for POST
  data.
