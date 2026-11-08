-- The subscription session list (store::session_refs::on_subscription)
-- looks up usage by agent, session and subscription for each session.
-- Without this index both lookups walked every usage sample of the
-- subscription per session and held the database lock for seconds.
CREATE INDEX usage_samples_session ON usage_samples(agent, session, subscription);
