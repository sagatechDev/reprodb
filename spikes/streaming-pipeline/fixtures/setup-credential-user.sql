SET @password_value = CONVERT(
    0x2063726564656e7469616c2d7365637265742d6d61726b6572202722233b5c0a090d20747261696c696e6720
    USING utf8mb4
);

SET @create_user = CONCAT(
    'CREATE USER \'credential_spike\'@\'%\' IDENTIFIED BY ',
    QUOTE(@password_value)
);

PREPARE create_user_statement FROM @create_user;
EXECUTE create_user_statement;
DEALLOCATE PREPARE create_user_statement;
