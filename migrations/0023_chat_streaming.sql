ALTER TABLE endpoints
    ADD COLUMN supports_streaming boolean NOT NULL DEFAULT false;

ALTER TABLE endpoints
    ADD CONSTRAINT streaming_contract CHECK (
        NOT supports_streaming OR operation = 'chat'
    );

