# The image that cuts a live venue's journal (interfaces/tools/journal_cut.py).
#
# One image because the procedure is one process: it stops the core, runs `venue-node` against the journal, ships
# the segment with Python and the AWS CLI, and starts the core again from a `finally`, which only works if all of
# those are in the same container. The existing images split them (the venue image has the binaries and no Python;
# the archive images are Alpine, so a glibc `venue-node` cannot run in them).
#
# It is built *on top of an existing venue image* rather than from source: the `venue-node` that embeds the
# snapshot must write the encoding the running core reads, and which image that is is a decision, not a default.
# Pass the venue image the core runs, or one with the same snapshot encoding, as BASE. Building on a tag means no
# Rust compile and a build of a minute or two.
#
#   docker build -f deploy/images/venue-cut.Dockerfile --build-arg BASE=ghcr.io/<owner>/venue:<tag> .
ARG BASE
FROM ${BASE}

USER root
RUN apt-get update \
    && apt-get install -y --no-install-recommends python3 awscli \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /opt/venue
COPY interfaces/tools/journal_cut.py interfaces/tools/journal_ship.py interfaces/tools/snapshot_publish.py \
     interfaces/tools/snapshot_fetch.py ./

# The venue image's own user, as the core runs: the journal volume is owned by it.
USER 10001
ENTRYPOINT ["python3", "/opt/venue/journal_cut.py"]
