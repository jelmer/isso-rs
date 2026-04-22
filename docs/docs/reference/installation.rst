Installation
============

Isso is distributed as the ``isso`` binary (a Rust rewrite of the original
Python server, wire-compatible with the upstream SQLite schema and JSON API).
There are three ways to run it: a pre-built Docker image, building from
source, or a pre-built distribution package.

.. contents::
    :local:
    :depth: 1

.. _install-from-source:

Build from source
-----------------

Requirements
^^^^^^^^^^^^

- Rust 1.70+ (stable toolchain — use `rustup <https://rustup.rs>`_ if your
  distribution's packaged toolchain is older)
- Node.js + npm — only for building the JavaScript client bundles; skip this
  if you deploy the pre-built ``static/`` from a release

Get a fresh checkout:

.. code-block:: console

    $ git clone https://github.com/isso-comments/isso.git
    $ cd isso/

Build the JavaScript bundles:

.. code-block:: console

    $ make init      # installs JS dependencies via npm
    $ make js        # runs webpack, writes static/js/embed.*.js

Build the server:

.. code-block:: console

    $ make build     # cargo build --release → target/release/isso

Copy or symlink the binary into your ``$PATH`` and the ``static/``,
``templates/`` trees + a copy of ``isso.cfg`` to a persistent
location. Static assets are only needed if you want ``isso`` to serve them
itself (set ``[server] static-dir = /path/to/static``); deployments behind a
reverse proxy commonly let the proxy serve them.

.. _using-docker:

Using Docker
------------

The repository's ``Dockerfile`` builds a three-stage image (Node, Rust,
Alpine runtime) that bundles the binary plus the ``static/`` and
``templates/`` trees.

.. code-block:: console

    $ make docker        # tags isso:latest locally
    $ mkdir -p config/ db/
    $ cp isso.cfg config/isso.cfg   # edit dbpath + host
    $ docker run -d --rm --name isso -p 127.0.0.1:8080:8080 \
        -v $PWD/config:/config -v $PWD/db:/db \
        isso:latest

The container's ``ENTRYPOINT`` is ``/usr/local/bin/isso -c /config/isso.cfg``,
so any extra flags you want to pass (``import``, ``-c /config/other.cfg``
for multi-site, etc.) go after the image name.

Upstream Python-port Docker images
^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

The upstream Python project publishes Docker images at
``ghcr.io/isso-comments/isso``. Those images run the Python server — the
Rust port's image is built from this repository's ``Dockerfile`` and must
be tagged and hosted separately.

.. _init-scripts:

Running as a service
--------------------

``isso`` is a plain binary that binds a TCP or Unix socket. Any
process supervisor works — systemd, OpenRC, runit, supervisord, etc.

Minimal systemd unit:

.. code-block:: ini

    [Unit]
    Description=Isso comment server
    After=network.target

    [Service]
    Type=simple
    User=isso
    ExecStart=/usr/local/bin/isso -c /etc/isso.cfg
    Restart=on-failure

    [Install]
    WantedBy=multi-user.target

For deployments behind nginx/caddy using a Unix socket, set
``listen = unix:///run/isso.sock`` under ``[server]`` and point the reverse
proxy at that socket path.

Upgrades
--------

From source: ``git pull && make build`` drops a new binary in
``target/release/isso``. Restart the service.

From Docker: rebuild via ``make docker`` (or pull the image tag you use)
and ``docker restart`` the container. The SQLite DB migrates forward
automatically on open — see the schema-version notes in
``docs/porting-reference.md``.
