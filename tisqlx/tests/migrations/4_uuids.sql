CREATE TABLE uuids (
    id BINARY(16) PRIMARY KEY,
    ext CHAR(36),
    digest BINARY(16) NOT NULL,
    short VARBINARY(16) NOT NULL
);
