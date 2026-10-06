# Glossary

- **Session model**: The model used by the normal conversation and composer.
- **Fusion Lead**: The model that plans and verifies a Fusion turn. Each session must explicitly select a valid Lead before Fusion can be enabled; it never falls back to the composer model.
- **Fusion Sidekick**: The model that performs delegated Fusion work. Each session must explicitly select a valid Sidekick before Fusion can be enabled; it never inherits the Lead.
- **Provider catalog**: The model IDs discovered for a provider and explicitly selected by the user. Only saved catalog entries are offered as provider model choices.
