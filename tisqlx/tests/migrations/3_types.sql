CREATE TABLE typed (
    id BIGINT PRIMARY KEY,
    tags SET('a', 'b', 'c') NOT NULL,
    flag BIT(1) NOT NULL,
    big DECIMAL(65, 30) NOT NULL,
    small DECIMAL(10, 2) NOT NULL,
    ubin BINARY(16)
);
