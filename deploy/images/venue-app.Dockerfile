# The retail app as a web page anybody can open, for a demo that needs no install and no account.
#
#   docker build -f deploy/images/venue-app.Dockerfile --build-arg VENUE_URL=https://venue.example -t venue-app:dev .
#
# **The venue's address is baked in here, not read at runtime.** Expo inlines every `EXPO_PUBLIC_*` variable into the
# bundle when it exports, so there is no environment variable to set on the pod later: an image built against one
# venue points at that venue for as long as it exists. Pointing the demo somewhere else means building again with a
# different `VENUE_URL`. What it does *not* mean is that a visitor is stuck — the app asks `GET /v4/bootstrap` for
# the venues it may point itself at, so the built-in address is where it starts, not where it must stay.

FROM node:22-bookworm-slim AS build
WORKDIR /src/apps/retail

# The manifest, the lockfile and the patches first, so a change to the app's own source does not reinstall
# node_modules. The patches are needed *at install time*, not after: `postinstall` runs patch-package, and without
# the directory here it silently finds nothing to apply and the build fails much later, inside Metro, complaining
# about something that looks unrelated.
COPY apps/retail/package.json apps/retail/package-lock.json ./
COPY apps/retail/patches/ ./patches/
RUN npm ci

COPY apps/retail/ ./

# Where the app looks for the venue. A build that did not say would quietly point at 127.0.0.1:8080 — the default in
# environment.ts, right for a laptop and useless in a browser somewhere else — and the page would load perfectly and
# then fail to fetch anything, which is the least helpful way for this to go wrong. So it is required.
ARG VENUE_URL
RUN test -n "$VENUE_URL" || (echo "VENUE_URL is required: the venue this demo talks to is baked into the bundle" >&2; exit 1)
ENV EXPO_PUBLIC_VENUE_URL=$VENUE_URL
RUN npx expo export --platform web

# nginx rather than `expo start`: what is served is a directory of static files, and a development server on the
# internet is a different thing with a different attack surface.
FROM nginx:1.29-alpine
COPY deploy/images/venue-app.nginx.conf /etc/nginx/conf.d/default.conf
COPY --from=build /src/apps/retail/dist /usr/share/nginx/html
# Runs unprivileged: nginx's own image supports this as long as it can write its temporary directories and listens
# above 1024, both of which the config below arranges.
RUN chown -R nginx:nginx /var/cache/nginx /usr/share/nginx/html
USER nginx
EXPOSE 8080
