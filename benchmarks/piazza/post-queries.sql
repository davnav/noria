QUERY post_count: select * from Post where p_author=?;
--QUERY post_count: select p_author, count(p_id) from Post where p_author=? group by p_author;
