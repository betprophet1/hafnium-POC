# The audit shipper (interfaces/tools/audit_ship.py) and enough around it to reach S3. Build from the repository
# root:
#   docker build -f deploy/images/venue-archive.Dockerfile -t venue-archive:dev .
#
# Alpine with its own packages rather than `amazon/aws-cli`, which carries a Python for the CLI's own use and does
# not put one on the path — the shipper is a Python program, so that image cannot run it.
#
# No credentials are built in or mounted. On an EC2 node the CLI reads the instance's role through the metadata
# service, which is why the instance has one and why its metadata hop limit has to allow a container to ask.
FROM alpine:3.21

RUN apk add --no-cache python3 aws-cli

# `audit_ship.py` imports `audit_verify.py` beside it: a file is only taken once it verifies, and that is the
# verifier the rest of the venue uses rather than a second opinion written for the shipper.
WORKDIR /opt/venue
COPY interfaces/tools/audit_ship.py interfaces/tools/audit_verify.py ./

# Nothing here needs to be root: it reads an audit volume, writes objects, and unlinks what it has confirmed.
RUN adduser -D -u 10001 archive
USER archive

ENTRYPOINT ["python3", "/opt/venue/audit_ship.py"]
