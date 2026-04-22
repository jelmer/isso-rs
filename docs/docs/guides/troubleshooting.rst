Troubleshooting
===============

For uberspace users
-------------------
Some uberspace users experienced problems with isso and they solved their
issues by adding `DirectoryIndex disabled` as the first line in the `.htaccess`
file for the domain the isso server is running on.

The `Installing Isso on Uberspace <https://lab.uberspace.de/guide_isso/>`_
guide should also be helpful.

``cargo build`` fails with a missing system dependency
------------------------------------------------------

``isso-rs``'s build depends on an available C toolchain and OpenSSL
headers. On Debian/Ubuntu::

    $ sudo apt-get install build-essential pkg-config libssl-dev

On Alpine (as used by the Docker image)::

    $ apk add musl-dev perl make

If ``cargo build`` still complains, make sure your Rust toolchain is
current — the project targets stable Rust 1.70 or newer. Use
`rustup <https://rustup.rs>`_ if your distribution's packaged toolchain
is older.

Why isn't markdown in my comments rendering as I expect?
--------------------------------------------------------

Please :ref:`configure <configure-markup>` Isso's markup parser to your
requirements as described in :doc:`/docs/reference/markdown-config`.

UnicodeDecodeError: 'ascii' codec can't decode byte 0xff
--------------------------------------------------------

Likely an issue with your environment, check you set your preferred file
encoding either in :envvar:`LANG`, :envvar:`LANGUAGE`, :envvar:`LC_ALL` or
:envvar:`LC_CTYPE`:

.. code-block:: text

    $ env LANG=C.UTF-8 isso [-h] [--version] ...

If none of the mentioned variables are set, the interaction with Isso will
likely fail (unable to print non-ascii characters to stdout/err, unable to
parse configuration file with non-ascii characters and so forth).

The web console shows 404 Not Found responses
---------------------------------------------

Isso returned "404 Not Found" to indicate "No comments" in versions prior to
0.12.3. This behaviour was changed in
`a pull request <https://github.com/isso-comments/isso/pull/565>`_ to return a code
of "200" with an empty array.
