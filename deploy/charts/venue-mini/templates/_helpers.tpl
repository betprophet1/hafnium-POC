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
