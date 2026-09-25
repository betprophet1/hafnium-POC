# The database archiver (interfaces/tools/db_archive.py) and enough around it to reach S3. Build from the
# repository root:
#   docker build -f deploy/images/venue-dbarchive.Dockerfile -t venue-dbarchive:dev .
#
# Based on the *same* PostgreSQL image the server runs, digest and all, rather than on Alpine with
# `apk add postgresql-client` the way venue-archive is built. `pg_dump` refuses to dump from a server newer than
# itself, and Alpine 3.21's postgresql-client is 17 against a server on 18.6 — so the obvious build produces an
# image that passes every test that does not touch a real database and then fails in the CronJob. Pinning to the
# server's own tag makes the versions equal by construction; when values.yaml moves the server, move this with
# it, and the failure mode if they ever drift is loud rather than silent.
FROM postgres:18.6-alpine3.24@sha256:d3e1620b530c944afa6e887d22eb899824da68e19c52024bf98f5220c88a65b2

RUN apk add --no-cache python3 aws-cli

# `db_archive.py` imports `snapshot_publish.py` beside it for the verified upload: an object is only reported
# archived once it has been read back and compared, and that is the same code the snapshot and journal shippers
# use rather than a third copy of the same care.
WORKDIR /opt/venue
COPY interfaces/tools/db_archive.py interfaces/tools/snapshot_publish.py ./

# No credentials are built in or mounted. On an EC2 node the CLI reads the instance's role through the metadata
# service; the database password arrives in the environment from the release's Secret, and never on a command
# line — `db_archive.py` passes it to pg_dump in PGPASSWORD so it cannot be read out of `ps`.
#
# Nothing here needs to be root, and nothing here needs to be the `postgres` user either: this only reads a
# database over TCP and writes objects.
RUN adduser -D -u 10001 dbarchive
USER dbarchive

ENTRYPOINT ["python3", "/opt/venue/db_archive.py"]
