# Input transform example
#
# Demonstrates (harness/replace-prompt new-text), which rewrites the
# user's prompt for the current turn before the agent sees it. The
# original message disappears from the LLM's view; only the replacement
# is sent.
#
# This example detects a "?fr " prefix on the user's input and rewrites
# the rest to force a French response. Port of the pi `input-transform`
# example.

(def hooks ["on-prompt"])

(defn on-prompt [ctx]
  (let [prompt (ctx :prompt)]
    (when (and (string? prompt) (string/has-prefix? "?fr " prompt))
      (let [rest (string/slice prompt 4)]
        (harness/replace-prompt
          (string "Répondez en français : " rest)))))
  nil)
