{{- define "venue-app.labels" -}}
app.kubernetes.io/name: venue-app
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version }}
{{- end -}}

{{- define "venue-app.selectorLabels" -}}
app.kubernetes.io/name: venue-app
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}
