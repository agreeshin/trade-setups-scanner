-- User research admission is independent of owner maintenance and live intent.
ALTER TABLE continuous_processes
    ADD COLUMN pause_requested boolean NOT NULL DEFAULT false;
