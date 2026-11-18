INSERT INTO posts (user_id, title) SELECT id, 'hello' FROM users WHERE name = 'alice';
