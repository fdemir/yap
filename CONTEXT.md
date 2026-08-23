# Yap

Yap is a terminal coding agent that runs model-guided work inside one canonical workspace while retaining explicit control over effects and data destinations.

## Language

**Provider profile**:
A user-named model connection target that identifies where model requests and workspace context are sent.
_Avoid_: Provider, vendor, backend

**Model reference**:
A provider profile and opaque model identifier written as `provider-profile/model-id`.
_Avoid_: Model name, provider model

**Selected model**:
The model reference and settings fixed for a single turn.
_Avoid_: Active provider, current backend

**Credential reference**:
A non-secret pointer to where a provider profile obtains its credential.
_Avoid_: API key, credential value
