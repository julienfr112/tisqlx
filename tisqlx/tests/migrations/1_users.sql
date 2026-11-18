CREATE TABLE users (
    id BIGINT PRIMARY KEY AUTO_RANDOM,
    name VARCHAR(64) NOT NULL,
    email VARCHAR(255),
    status ENUM('active', 'banned') NOT NULL DEFAULT 'active',
    score DECIMAL(10, 2) NOT NULL DEFAULT 0,
    profile JSON,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)
);
-- comments after the last statement are fine
