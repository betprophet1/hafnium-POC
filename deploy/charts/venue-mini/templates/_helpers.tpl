{{- define "venue-mini.fullname" -}}
{{- default .Release.Name .Values.fullnameOverride | trunc 50 | trimSuffix "-" -}}
{{- end -}}

{{- define "venue-mini.image" -}}
{{ .Values.image.repository }}:{{ .Values.image.tag }}
{{- end -}}

{{- define "venue-mini.labels" -}}
app.kubernetes.io/name: venue-mini
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version }}
{{- end -}}

{{- define "venue-mini.coreSelector" -}}
app.kubernetes.io/name: venue-mini
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/component: core
{{- end -}}

{{- define "venue-mini.selector" -}}
app.kubernetes.io/name: venue-mini
app.kubernetes.io/instance: {{ .ctx.Release.Name }}
app.kubernetes.io/component: {{ .component }}
{{- end -}}

{{- define "venue-mini.podSecurity" -}}
runAsNonRoot: true
runAsUser: 10001
runAsGroup: 10001
fsGroup: 10001
{{- end -}}

{{- define "venue-mini.containerSecurity" -}}
allowPrivilegeEscalation: false
readOnlyRootFilesystem: true
capabilities:
  drop: ["ALL"]
{{- end -}}

{{/*
A gateway pod's spec but its volumes, which name `catalogue` and `audit`: the halves of interface 1 given by `serve`
(both, public or private), with `ctx` the chart's root. A private half reaches the core's ingress and the services it
forwards to; a public one only follows the feed. Each pod of a split gateway writes the audit instance of its own name.
*/}}
{{- define "venue-mini.gatewayPod" -}}
{{- $ := .ctx }}
{{- $name := include "venue-mini.fullname" $ }}
{{- $core := printf "%s-core" $name -}}
securityContext:
  {{- include "venue-mini.podSecurity" $ | nindent 2 }}
# A stopping gateway first drains (preStop), then gets SIGTERM, which ends it at once: its sessions close, and their
# clients reconnect through the Service to another pod.
terminationGracePeriodSeconds: {{ add $.Values.gateway.drainSeconds 10 }}
containers:
  - name: gateway
    image: {{ include "venue-mini.image" $ | quote }}
    imagePullPolicy: {{ $.Values.image.pullPolicy }}
    command: ["tini", "--", "venue-gateway"]
    env:
      - name: VENUE_LISTEN
        value: "0.0.0.0:8081"
      {{- if ne .serve "both" }}
      - name: VENUE_SERVE
        value: {{ .serve }}
      {{- end }}
      {{- if ne .serve "public" }}
      - name: VENUE_INGRESS
        value: "{{ $core }}:7101"
      {{- end }}
      - name: VENUE_FEED
        value: "{{ $core }}:7102"
      - name: VENUE_CATALOGUE
        value: /catalogue/markets.json
      {{- if ne .serve "public" }}
      {{- if $.Values.treasury.enabled }}
      - name: VENUE_TREASURY
        value: "{{ $name }}-treasury:8083"
      {{- end }}
      {{- if $.Values.identity.enabled }}
      - name: VENUE_IDENTITY
        value: "{{ $name }}-identity:8084"
      {{- end }}
      {{- if $.Values.parlays.enabled }}
      - name: VENUE_PARLAYS
        value: "{{ $name }}-parlays:7103"
      {{- end }}
      {{- end }}
      - name: VENUE_PER_SECOND
        value: {{ $.Values.gateway.perSecond | quote }}
      - name: VENUE_BURST
        value: {{ $.Values.gateway.burst | quote }}
      - name: VENUE_LOGONS_PER_MINUTE
        value: {{ $.Values.gateway.logonsPerMinute | quote }}
      - name: VENUE_AUDIT_DIR
        value: /var/lib/gateway-audit
      - name: VENUE_AUDIT_INSTANCE
      {{- if eq .serve "both" }}
        value: "{{ $name }}-gateway"
      {{- else }}
        valueFrom:
          fieldRef:
            fieldPath: metadata.name
      {{- end }}
      - name: VENUE_AUDIT_FSYNC
        value: {{ $.Values.gateway.audit.fsync | quote }}
      {{- if $.Values.metrics.enabled }}
      - name: VENUE_METRICS
        value: "0.0.0.0:9102"
      - name: VENUE_LOG_FORMAT
        value: {{ $.Values.metrics.logFormat | quote }}
      {{- end }}
      {{- if and $.Values.ingress.enabled $.Values.ingress.trustedProxies }}
      # Behind the ingress every connection comes from its pods: their X-Forwarded-For names the client.
      - name: VENUE_TRUSTED_PROXIES
        value: {{ $.Values.ingress.trustedProxies | quote }}
      {{- end }}
    ports:
      - name: http
        containerPort: 8081
      {{- if $.Values.metrics.enabled }}
      - name: metrics
        containerPort: 9102
      {{- end }}
    # Taken out of the Service's endpoints as soon as it is stopping; the ingress and kube-proxy stop picking it while
    # it still answers.
    lifecycle:
      preStop:
        sleep:
          seconds: {{ $.Values.gateway.drainSeconds }}
    # Ready once the gateway has caught up with the core's feed.
    readinessProbe:
      httpGet:
        path: /healthz
        port: http
      periodSeconds: 3
    resources:
      requests:
        cpu: 20m
        memory: 32Mi
    securityContext:
      {{- include "venue-mini.containerSecurity" $ | nindent 6 }}
    volumeMounts:
      - name: catalogue
        mountPath: /catalogue
      - name: audit
        mountPath: /var/lib/gateway-audit
{{- end -}}

{{/*
The services that keep their records in PostgreSQL and are enabled, in the order migrations run.
*/}}
{{- define "venue-mini.databaseServices" -}}
{{- $services := list -}}
{{- range $service := list "treasury" "identity" "parlays" "oracle" -}}
{{- if (index $.Values $service).enabled -}}
{{- $services = append $services $service -}}
{{- end -}}
{{- end -}}
{{- join " " $services -}}
{{- end -}}

{{- define "venue-mini.postgresSecret" -}}
{{- .Values.postgres.existingSecret | default (printf "%s-postgres" (include "venue-mini.fullname" .)) -}}
{{- end -}}

{{/*
The environment that tells a service where its database is: `service` one of treasury, identity, parlays, oracle, and
`ctx` the chart's root. In-cluster, the URL is put together from the service's generated password (URL-safe); outside,
it comes whole from a Secret.
*/}}
{{- define "venue-mini.databaseEnv" -}}
{{- $ := .ctx -}}
{{- $db := index $.Values.database .service -}}
{{- if $.Values.postgres.enabled }}
- name: DATABASE_PASSWORD
  valueFrom:
    secretKeyRef:
      name: {{ include "venue-mini.postgresSecret" $ }}
      key: {{ .service }}-password
- name: VENUE_DATABASE_URL
  value: "postgres://{{ .service }}:$(DATABASE_PASSWORD)@{{ include "venue-mini.fullname" $ }}-postgres:5432/{{ .service }}"
# The pod network, confined by network policy to the owning services.
- name: VENUE_DATABASE_TLS
  value: disable
{{- else }}
- name: VENUE_DATABASE_URL
  valueFrom:
    secretKeyRef:
      {{- if $db.existingSecret }}
      name: {{ $db.existingSecret }}
      key: {{ $db.existingSecretKey }}
      {{- else }}
      name: {{ include "venue-mini.fullname" $ }}-database
      key: {{ .service }}-url
      {{- end }}
- name: VENUE_DATABASE_TLS
  value: {{ $.Values.database.tls | quote }}
{{- if $.Values.database.caSecret }}
- name: VENUE_DATABASE_CA_FILE
  value: /etc/venue/database-ca/ca.pem
{{- end }}
{{- end }}
- name: VENUE_DATABASE_SCHEMA
  value: {{ .service }}
- name: VENUE_DATABASE_POOL_SIZE
  value: {{ $.Values.database.poolSize | quote }}
{{- end -}}

{{/* The CA bundle's volume mount and volume, for external databases that give one. */}}
{{- define "venue-mini.databaseCaMount" -}}
{{- if and (not .Values.postgres.enabled) .Values.database.caSecret }}
- name: database-ca
  mountPath: /etc/venue/database-ca
  readOnly: true
{{- end }}
{{- end -}}

{{- define "venue-mini.databaseCaVolume" -}}
{{- if and (not .Values.postgres.enabled) .Values.database.caSecret }}
- name: database-ca
  secret:
    secretName: {{ .Values.database.caSecret }}
{{- end }}
{{- end -}}
