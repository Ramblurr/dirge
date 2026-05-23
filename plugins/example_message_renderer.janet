# Plugin-registered message renderer example (P9d).
#
# `harness/register-message-renderer type handler` provides a Janet
# function the UI invokes when it sees a `LoopMessage::Custom` event
# whose payload's `type` field matches. Pi-style API mirror —
# `api.registerMessageRenderer(customType, renderer)`.
#
# Custom messages enter the loop via `harness/add-custom-message`
# (typically from a `prepare-next-run` or `on-turn-end` hook). They
# pass through `convert_to_llm` as UI-only — the LLM never sees them.
# Without a registered renderer the UI falls back to extracting the
# payload's `content` field, or stringifying the whole payload.
#
# Handlers receive the raw JSON payload as a single string argument.
# They return the display text. Distinct from `harness/register-renderer`
# (which is for session-timeline plugin entries — bookmarks, etc.) in
# that message renderers fire LIVE mid-conversation.

(defn render-status [payload]
  # `payload` is the wrapper JSON string carrying customType, content,
  # display. For demo purposes we just label and forward it. Real
  # renderers would parse the JSON (json/decode is not bundled with
  # the embedded Janet — extract fields via string ops or pass the
  # data shape your plugin controls).
  (string "■ status from plugin: " payload))

(harness/register-message-renderer "status" "render-status")

# Emit a custom message every turn to demo the rendering path. This
# uses `prepare-next-run` (fires after the LLM emits Done and before
# the next user prompt is collected) so it doesn't spam mid-stream.
# Three-arg form: customType, content, &opt display. `display=false`
# would suppress the chat line while keeping the entry in the
# transcript.
(defn prepare-next-run [ctx]
  (harness/add-custom-message "status" "another turn complete"))
