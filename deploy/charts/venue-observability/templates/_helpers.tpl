{{- define "venue-observability.labels" -}}
app.kubernetes.io/name: venue-observability
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version }}
{{- end -}}

{{/* One Grafana data source entry: `ds` its values, `type` its Grafana type. */}}
{{- define "venue-observability.datasource" -}}
- name: {{ .ds.name }}
  uid: {{ .ds.uid }}
  type: {{ .type }}
  access: proxy
  url: {{ .ds.url }}
  editable: false
  {{- with .ds.jsonData }}
  jsonData:
    {{- toYaml . | nindent 4 }}
  {{- end }}
  {{- if .ds.basicAuthUser }}
  basicAuth: true
  basicAuthUser: {{ .ds.basicAuthUser }}
  secureJsonData:
    basicAuthPassword: ${{ printf "{%s}" .ds.basicAuthPasswordEnv }}
  {{- end }}
{{- end -}}
