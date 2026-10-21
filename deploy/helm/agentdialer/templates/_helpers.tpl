{{- define "agentdialer.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "agentdialer.fullname" -}}
{{- printf "%s" (include "agentdialer.name" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "agentdialer.labels" -}}
app.kubernetes.io/name: {{ include "agentdialer.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version }}
{{- end -}}

{{- define "agentdialer.image" -}}
{{ .Values.image.repository }}:{{ .Values.image.tag }}
{{- end -}}

{{/* DATABASE_URL from a Secret (preferred) or an inline value (dev). */}}
{{- define "agentdialer.dbEnv" -}}
- name: DATABASE_URL
{{- if .Values.database.existingSecret }}
  valueFrom:
    secretKeyRef:
      name: {{ .Values.database.existingSecret }}
      key: DATABASE_URL
{{- else }}
  value: {{ .Values.database.url | quote }}
{{- end }}
{{- end -}}

{{/* The LiveKit API key and secret, from a Secret (preferred) or inline values
(dev). The orchestrator needs them; the api uses them to verify webhooks. */}}
{{- define "agentdialer.livekitKeysEnv" -}}
{{- if .Values.livekit.existingSecret }}
- name: LIVEKIT_API_KEY
  valueFrom:
    secretKeyRef:
      name: {{ .Values.livekit.existingSecret }}
      key: LIVEKIT_API_KEY
- name: LIVEKIT_API_SECRET
  valueFrom:
    secretKeyRef:
      name: {{ .Values.livekit.existingSecret }}
      key: LIVEKIT_API_SECRET
{{- else }}
- name: LIVEKIT_API_KEY
  value: {{ .Values.livekit.apiKey | quote }}
- name: LIVEKIT_API_SECRET
  value: {{ .Values.livekit.apiSecret | quote }}
{{- end }}
{{- end -}}

{{/* LiveKit env: the rooms where the calls meet the agents. */}}
{{- define "agentdialer.livekitEnv" -}}
- name: LIVEKIT_URL
  value: {{ .Values.livekit.url | quote }}
- name: AD_ROOM_PREFIX
  value: {{ .Values.livekit.roomPrefix | quote }}
{{- include "agentdialer.livekitKeysEnv" . }}
{{- end -}}
