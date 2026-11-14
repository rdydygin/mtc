//! Real Mattermost contracts. Run serially with MTC_LIVE_SERVER and --ignored.
//! Provisioning/configuration targets a disposable test server (never production).
mod support;

use anyhow::Result;
use mattermost::{Client, CustomStatus, Event, FAVORITE_CHANNEL, Preference, Status};
use serde_json::{Value, json};
use support::{Live, connected, event, raw};

#[tokio::test]
#[ignore = "requires MTC_LIVE_SERVER; modifies a disposable Mattermost server"]
async fn identity_discovery_channels_and_commands() -> Result<()> {
    let Some(f) = Live::provision().await? else {
        return Ok(());
    };
    let (_ws, mut events) = connected(&f.a).await;
    let me = f.a.me().await?;
    assert_eq!(me.id, f.alice.id);
    assert!(
        !me.notify_props.is_empty(),
        "users/me must include notification defaults: {me:?}"
    );
    let (resumed, user) = Client::with_token(f.a.server().as_str(), f.a.token()).await?;
    assert_eq!(user.id, me.id);
    assert_eq!(resumed.server(), f.a.server());
    assert!(f.a.my_teams().await?.iter().any(|t| t.id == f.team.id));
    let channels = f.a.my_channels(&f.team.id).await?;
    for channel in [&f.public, &f.private] {
        assert!(
            channels.iter().any(|c| c.id == channel.id),
            "missing {channel:?}: {channels:?}"
        );
        assert_eq!(f.a.channel(&channel.id).await?.id, channel.id);
        assert_eq!(f.a.my_channel_member(&channel.id).await?.user_id, me.id);
    }
    assert!(
        f.a.my_channel_members(&f.team.id)
            .await?
            .iter()
            .any(|m| m.channel_id == f.public.id)
    );
    assert_eq!(
        f.a.users_by_ids(vec![f.bob.id.clone(), f.carol.id.clone()])
            .await?
            .len(),
        2
    );
    assert_eq!(
        f.a.users_by_usernames(vec![f.bob.username.clone(), f.carol.username.clone()])
            .await?
            .len(),
        2
    );
    assert!(
        f.a.search_users(&f.bob.username, 10)
            .await?
            .iter()
            .any(|u| u.id == f.bob.id)
    );
    let ac =
        f.a.autocomplete_users(&f.team.id, &f.public.id, "mtclive")
            .await?;
    assert!(
        ac.users().iter().any(|u| u.id == f.bob.id),
        "autocomplete in_channel: {ac:?}"
    );
    assert!(
        ac.out_of_channel().iter().any(|u| u.id == f.carol.id),
        "autocomplete out_of_channel: {ac:?}"
    );
    let publics = f.a.public_channels(&f.team.id, 0, 100).await?;
    assert!(publics.iter().any(|c| c.id == f.public.id));
    assert!(!publics.iter().any(|c| c.id == f.private.id));
    assert!(
        f.a.search_public_channels(&f.team.id, &f.public.name)
            .await?
            .iter()
            .any(|c| c.id == f.public.id)
    );
    f.c.join_channel(&f.public.id, &f.carol.id).await?;
    event(
        &mut events,
        "user_added",
        |e| matches!(e,Event::ChannelsChanged {name,..} if name=="user_added"),
    )
    .await;
    f.c.leave_channel(&f.public.id, &f.carol.id).await?;
    event(
        &mut events,
        "user_removed",
        |e| matches!(e,Event::ChannelsChanged {name,..} if name=="user_removed"),
    )
    .await;
    let dm = f.a.direct_channel(&me.id, &f.bob.id).await?;
    event(
        &mut events,
        "direct_added",
        |e| matches!(e,Event::ChannelsChanged{name,..} if name=="direct_added"),
    )
    .await;
    assert_eq!(f.a.direct_channel(&me.id, &f.bob.id).await?.id, dm.id);
    let users = [me.id.clone(), f.bob.id.clone(), f.carol.id.clone()];
    let group = f.a.group_channel(&users).await?;
    event(
        &mut events,
        "group_added",
        |e| matches!(e,Event::ChannelsChanged{name,..} if name=="group_added"),
    )
    .await;
    assert_eq!(
        f.a.group_channel(&[users[2].clone(), users[0].clone(), users[1].clone()])
            .await?
            .id,
        group.id
    );
    let members = f.a.group_channel_members(vec![group.id.clone()]).await?;
    // /users/group_channels excludes the authenticated user (run-1 line 1488).
    let mut participants: Vec<_> = members
        .get(&group.id)
        .expect("group users response keyed by channel")
        .iter()
        .map(|u| u.id.clone())
        .collect();
    participants.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    let mut expected = vec![f.bob.id.clone(), f.carol.id.clone()];
    expected.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    assert_eq!(participants, expected);
    let commands = f.a.autocomplete_commands(&f.team.id).await?;
    assert!(
        commands.iter().any(|c| c.trigger() == "shrug"),
        "built-in commands missing: {commands:?}"
    );
    f.a.execute_command(&f.public.id, &f.team.id, "/shrug mtc live command")
        .await?;
    assert!(!f.a.user_image(&me.id).await?.is_empty());
    // Logout a separate session token; leave fixture clients usable.
    let (temporary, _) = Client::login(
        f.a.server().as_str(),
        &f.carol.username,
        support::PASSWORD,
        None,
    )
    .await?;
    temporary.logout().await?;
    assert!(
        temporary.me().await.is_err(),
        "logout must invalidate the token"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires MTC_LIVE_SERVER; modifies a disposable Mattermost server"]
async fn posts_reactions_preferences_search_and_files() -> Result<()> {
    let Some(f) = Live::provision().await? else {
        return Ok(());
    };
    let (_ws, mut events) = connected(&f.a).await;
    let (bob_ws, mut bob_events) = connected(&f.b).await;
    let pending = format!("{}:{}", f.alice.id, f.suffix);
    let props = json!({"mtc_pending_id":pending})
        .as_object()
        .unwrap()
        .clone();
    let term = format!("mtccontract{}", f.suffix);
    let post =
        f.a.create_post(
            &f.public.id,
            &format!("{term} @{}", f.bob.username),
            None,
            &pending,
            &[],
            &props,
        )
        .await?;
    assert_eq!(
        post.pending_post_id, pending,
        "POST response must echo pending_post_id"
    );
    let echo = event(
        &mut events,
        "posted echo with pending id",
        |e| matches!(e,Event::Posted{post:p,..} if p.id==post.id),
    )
    .await;
    match echo {
        Event::Posted {
            post: p,
            team_id,
            mentions,
        } => {
            assert_eq!(p.pending_post_id, pending);
            assert_eq!(p.props.get("mtc_pending_id"), Some(&json!(pending)));
            assert_eq!(team_id, f.team.id);
            // The sender's scoped copy omits mentions. Assert mention decoding
            // on the recipient's stream below, as notifications do in Session.
            assert!(mentions.is_empty(), "sender-scoped mentions: {mentions:?}");
        }
        _ => unreachable!(),
    }
    let recipient = event(
        &mut bob_events,
        "recipient posted mention",
        |e| matches!(e,Event::Posted{post:p,..} if p.id==post.id),
    )
    .await;
    if let Event::Posted { mentions, .. } = recipient {
        assert_eq!(
            mentions,
            vec![f.bob.id.clone()],
            "recipient must receive mention user ids"
        );
    }
    let next = f.post(&f.b, "second root").await?;
    assert_eq!(f.a.get_post(&post.id).await?.message, post.message);
    assert_eq!(f.a.post_by_id(&post.id).await?.id, post.id);
    assert!(
        f.a.latest_posts(&f.public.id, 50)
            .await?
            .posts
            .contains_key(&next.id)
    );
    assert!(
        f.a.posts_before(&f.public.id, &next.id, 50)
            .await?
            .posts
            .contains_key(&post.id)
    );
    let since = f.a.posts_since(&f.public.id, post.create_at - 1).await?;
    assert_eq!(
        since.posts[&post.id].props.get("mtc_pending_id"),
        Some(&json!(pending))
    );
    f.a.edit_post(&post.id, &format!("{term} edited")).await?;
    event(
        &mut events,
        "post_edited",
        |e| matches!(e,Event::PostEdited(p) if p.id==post.id && p.message.ends_with("edited")),
    )
    .await;
    f.a.add_reaction(&f.alice.id, &post.id, "smile").await?;
    event(&mut events,"reaction_added",|e|matches!(e,Event::ReactionAdded(r) if r.post_id==post.id && r.emoji_name=="smile" && r.user_id==f.alice.id)).await;
    f.a.remove_reaction(&f.alice.id, &post.id, "smile").await?;
    event(
        &mut events,
        "reaction_removed",
        |e| matches!(e,Event::ReactionRemoved(r) if r.post_id==post.id && r.emoji_name=="smile"),
    )
    .await;
    for pinned in [true, false] {
        f.a.set_post_pinned(&post.id, pinned).await?;
        event(
            &mut events,
            "pin post_edited",
            |e| matches!(e,Event::PostEdited(p) if p.id==post.id && p.is_pinned==pinned),
        )
        .await;
        assert_eq!(
            f.a.pinned_posts(&f.public.id)
                .await?
                .posts
                .contains_key(&post.id),
            pinned
        );
    }
    let prefs = vec![
        Preference::new(
            f.alice.id.clone(),
            FAVORITE_CHANNEL,
            f.public.id.as_str(),
            "true",
        ),
        Preference::new(f.alice.id.clone(), "flagged_post", post.id.as_str(), "true"),
    ];
    f.a.save_preferences(&f.alice.id, prefs.clone()).await?;
    event(
        &mut events,
        "preferences_changed",
        |e| matches!(e,Event::PreferencesChanged(p) if prefs.iter().all(|want| p.contains(want))),
    )
    .await;
    assert!(
        f.a.my_preferences(FAVORITE_CHANNEL)
            .await?
            .contains(&prefs[0])
    );
    assert!(
        f.a.flagged_posts(&f.alice.id, 0, 50)
            .await?
            .posts
            .contains_key(&post.id)
    );
    f.a.delete_preferences(&f.alice.id, prefs.clone()).await?;
    event(
        &mut events,
        "preferences_deleted",
        |e| matches!(e,Event::PreferencesDeleted(p) if prefs.iter().all(|want|p.contains(want))),
    )
    .await;
    assert!(
        !f.a.flagged_posts(&f.alice.id, 0, 50)
            .await?
            .posts
            .contains_key(&post.id)
    );
    for terms in [term.clone(), format!("{term} in:{}", f.public.name)] {
        let found = f.a.search_posts(&f.team.id, &terms, 0, 30).await?;
        assert!(
            found.posts.iter().any(|p| p.id == post.id),
            "search {terms}: {found:?}"
        );
    }
    bob_ws.send_typing(&f.public.id, Some(&post.id));
    event(&mut events,"typing",|e|matches!(e,Event::Typing{channel_id,parent_id,user_id} if channel_id==&f.public.id && parent_id==&post.id && user_id==&f.bob.id)).await;
    let file =
        f.a.upload_file(&f.public.id, "pixel.png".into(), support::PNG.to_vec())
            .await?;
    assert!(file.is_image(), "PNG MIME type: {file:?}");
    assert_eq!(
        (file.width, file.height),
        (1024, 768),
        "uploaded PNG dimensions"
    );
    assert!(
        file.has_preview_image,
        "large PNG must have a preview: {file:?}"
    );
    let attachment =
        f.a.create_post(
            &f.public.id,
            "attachment",
            None,
            "",
            std::slice::from_ref(&file.id),
            &Default::default(),
        )
        .await?;
    let info: Value = raw(&f.a, "GET", &format!("files/{}/info", file.id), Value::Null).await?;
    assert_eq!(info["id"], file.id);
    assert_eq!(f.a.download_file(&file.id).await?, support::PNG);
    let image_url = f.a.file_image_url(&file);
    assert!(
        image_url.ends_with(&format!("/files/{}/preview", file.id)),
        "preview URL: {image_url}"
    );
    assert_eq!(f.a.fetch(&image_url).await?.status, 200);
    assert!(!f.a.file_thumbnail(&file.id).await?.is_empty());
    let hydrated = f.a.get_post(&attachment.id).await?;
    assert!(
        hydrated.metadata.files.iter().any(|i| i.id == file.id),
        "post file metadata: {hydrated:?}"
    );
    // LIVE: external OpenGraph scraping is deliberately excluded: CI has no
    // controlled public HTML origin. File metadata above exercises metadata.
    f.a.delete_post(&next.id).await?;
    let deleted = event(
        &mut events,
        "post_deleted identity (event payload may be the pre-deletion snapshot)",
        |e| matches!(e,Event::PostDeleted(p) if p.id==next.id),
    )
    .await;
    if let Event::PostDeleted(deleted) = deleted {
        assert_eq!(deleted.channel_id, next.channel_id);
        assert_eq!(deleted.user_id, next.user_id);
        // Run-2 server frames contain delete_at=0: the event itself confirms
        // deletion. The persisted REST tombstone is a separate contract.
        assert_eq!(
            deleted.delete_at, 0,
            "post_deleted contains the pre-deletion snapshot"
        );
    }
    let catchup = f.a.posts_since(&f.public.id, post.create_at - 1).await?;
    assert!(
        catchup.posts.get(&next.id).is_some_and(|p| p.delete_at > 0),
        "posts_since must return persisted deletion for {}: {catchup:?}",
        next.id
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires MTC_LIVE_SERVER; modifies a disposable Mattermost server"]
async fn read_state_crt_threads_and_notification_events() -> Result<()> {
    let Some(f) = Live::provision().await? else {
        return Ok(());
    };
    let (_ws, mut events) = connected(&f.a).await;
    let pref = Preference::new(
        f.alice.id.clone(),
        "display_settings",
        "collapsed_reply_threads",
        "on",
    );
    f.a.save_preferences(&f.alice.id, vec![pref]).await?;
    let root = f.post(&f.b, "followed thread root").await?;
    f.a.set_thread_following(&f.alice.id, &f.team.id, &root.id, true)
        .await?;
    event(&mut events,"thread_follow_changed",|e|matches!(e,Event::ThreadFollowChanged {thread_id,following:true,..} if thread_id==&root.id)).await;
    let reply =
        f.b.create_post(
            &f.public.id,
            &format!("reply @{}", f.alice.username),
            Some(&root.id),
            "",
            &[],
            &Default::default(),
        )
        .await?;
    event(&mut events,"thread_updated",|e|matches!(e,Event::ThreadUpdated {thread,..} if thread.id==root.id && thread.unread_replies>0)).await;
    assert!(f.a.thread(&root.id).await?.posts.contains_key(&reply.id));
    let thread =
        f.a.followed_thread(&f.alice.id, &f.team.id, &root.id)
            .await?;
    assert!(
        thread.unread_replies > 0,
        "followed thread unread: {thread:?}"
    );
    let unread =
        f.a.followed_threads(&f.alice.id, &f.team.id, None, true, 30)
            .await?;
    assert!(unread.threads.iter().any(|t| t.id == root.id));
    assert!(unread.total_unread_threads > 0);
    assert!(
        f.a.followed_thread_totals(&f.alice.id, &f.team.id)
            .await?
            .total_unread_mentions
            > 0
    );
    // This is the reason collapsed_threads_supported must be sent by mtc.
    // LIVE: a suspected Session gap remains unconfirmed: ChannelEntry::is_unread
    // uses max(last_root_post_at, last_post_at), which may treat reply-only
    // activity as channel unread even with CRT. Do not weaken this server
    // contract if a later Session assertion exposes that distinction.
    f.a.view_channel(&f.public.id).await?;
    event(&mut events,"channel viewed",|e|matches!(e,Event::ChannelViewed(id) if id==&f.public.id) || matches!(e,Event::ChannelsViewed(times) if times.iter().any(|(id,t)| id==&f.public.id && *t>0))).await;
    let after =
        f.a.followed_thread(&f.alice.id, &f.team.id, &root.id)
            .await?;
    assert_eq!(
        after.unread_replies, thread.unread_replies,
        "view_channel with CRT must preserve unread replies"
    );
    f.a.mark_thread_read(&f.alice.id, &f.team.id, &root.id, reply.create_at + 1)
        .await?;
    event(&mut events,"thread_read_changed",|e|matches!(e,Event::ThreadRead{thread_id:Some(id),timestamp,..} if id==&root.id && *timestamp>0)).await;
    assert_eq!(
        f.a.followed_thread(&f.alice.id, &f.team.id, &root.id)
            .await?
            .unread_replies,
        0
    );
    let page =
        f.a.followed_threads(&f.alice.id, &f.team.id, Some(&root.id), false, 30)
            .await?;
    assert!(
        !page.threads.iter().any(|t| t.id == root.id),
        "thread cursor must exclude itself: {page:?}"
    );
    f.a.mark_all_threads_read(&f.alice.id, &f.team.id).await?;
    assert_eq!(
        f.a.followed_thread_totals(&f.alice.id, &f.team.id)
            .await?
            .total_unread_threads,
        0
    );
    f.a.set_thread_following(&f.alice.id, &f.team.id, &root.id, false)
        .await?;
    event(&mut events,"unfollow",|e|matches!(e,Event::ThreadFollowChanged{thread_id,following:false,..} if thread_id==&root.id)).await;
    assert!(
        !f.a.followed_threads(&f.alice.id, &f.team.id, None, false, 30)
            .await?
            .threads
            .iter()
            .any(|t| t.id == root.id)
    );
    let post = f.post(&f.b, "mark this root unread").await?;
    let state = f.a.mark_post_unread(&f.alice.id, &post.id).await?;
    assert_eq!(state.channel_id, f.public.id);
    assert!(
        state.msg_count > 0 && state.msg_count_root > 0,
        "set_unread must return already-read counters for preceding roots: {state:?}"
    );
    let received = event(
        &mut events,
        "post_unread",
        |e| matches!(e,Event::PostUnread{post_id,..} if post_id==&post.id),
    )
    .await;
    if let Event::PostUnread { state: ws, .. } = received {
        assert_eq!(ws.channel_id, state.channel_id);
        assert_eq!(ws.user_id, f.alice.id);
        assert_eq!(ws.last_viewed_at, state.last_viewed_at);
        assert_eq!(
            (
                ws.msg_count,
                ws.msg_count_root,
                ws.mention_count,
                ws.mention_count_root
            ),
            (
                state.msg_count,
                state.msg_count_root,
                state.mention_count,
                state.mention_count_root
            ),
            "REST/WS unread counter shape"
        );
    }
    for muted in [true, false] {
        f.a.set_channel_muted(&f.public.id, &f.alice.id, muted)
            .await?;
        // PUT acknowledges with {"status":"OK"}; GET owns member state.
        let member = f.a.my_channel_member(&f.public.id).await?;
        let expected = if muted { "mention" } else { "all" };
        assert_eq!(
            member.notify_props.get("mark_unread").map(String::as_str),
            Some(expected)
        );
        event(&mut events,"channel_member_updated",|e|matches!(e,Event::ChannelMemberUpdated(m) if m.channel_id==f.public.id && m.notify_props.get("mark_unread").map(String::as_str)==Some(expected))).await;
    }
    // Trigger both viewed-event variants. channel_times is a map of ids to millis.
    let _: Value = raw(&f.a,"POST","channels/members/me/view",json!({"channel_id":f.private.id,"prev_channel_id":f.public.id,"collapsed_threads_supported":true})).await?;
    event(&mut events,"multiple_channels_viewed channel_times",|e|matches!(e,Event::ChannelsViewed(times) if times.iter().any(|(id,t)|id==&f.public.id && *t>0))).await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires MTC_LIVE_SERVER; modifies a disposable Mattermost server"]
async fn presence_emoji_and_websocket_reconnect() -> Result<()> {
    let Some(f) = Live::provision().await? else {
        return Ok(());
    };
    let (_ws, mut events) = connected(&f.a).await;
    f.a.set_status(&f.alice.id, Status::DoNotDisturb).await?;
    event(&mut events,"status_change",|e|matches!(e,Event::StatusChanged{user_id,status:Status::DoNotDisturb} if user_id==&f.alice.id)).await;
    assert!(
        f.a.statuses_by_ids(vec![f.alice.id.clone()])
            .await?
            .contains(&(f.alice.id.clone(), Status::DoNotDisturb))
    );
    f.a.set_custom_status(CustomStatus::new("calendar", "Live contract"))
        .await?;
    event(&mut events,"user_updated custom status",|e|matches!(e,Event::UserUpdated(u) if u.id==f.alice.id && u.custom_status("").is_some_and(|s|s.text=="Live contract"))).await;
    assert_eq!(
        f.a.me().await?.custom_status("").unwrap().text,
        "Live contract"
    );
    f.a.clear_custom_status().await?;
    event(
        &mut events,
        "user_updated cleared",
        |e| matches!(e,Event::UserUpdated(u) if u.id==f.alice.id && u.custom_status("").is_none()),
    )
    .await;
    // Skip only when the actual config explicitly disables custom emoji.
    let config: Value = raw(&f.admin, "GET", "config", Value::Null).await?;
    if config["ServiceSettings"]["EnableCustomEmoji"] == false {
        eprintln!("SKIP custom emoji: server config EnableCustomEmoji=false");
    } else {
        let name = format!("live{}", f.suffix);
        let form = reqwest::multipart::Form::new()
            .text(
                "emoji",
                json!({"name":name,"creator_id":f.alice.id}).to_string(),
            )
            .part(
                "image",
                reqwest::multipart::Part::bytes(support::PNG.to_vec())
                    .file_name("pixel.png")
                    .mime_str("image/png")?,
            );
        let response = reqwest::Client::new()
            .post(f.a.server().join("api/v4/emoji")?)
            .bearer_auth(f.a.token())
            .multipart(form)
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await?;
        assert!(
            status.is_success(),
            "POST /emoji multipart name={name}: {status}: {body}"
        );
        let emoji: mattermost::Emoji = serde_json::from_str(&body)?;
        event(
            &mut events,
            "emoji_added",
            |e| matches!(e,Event::EmojiAdded(e) if e.id==emoji.id),
        )
        .await;
        assert!(
            f.a.custom_emoji(0, 200)
                .await?
                .iter()
                .any(|e| e.id == emoji.id)
        );
        assert!(
            f.a.search_emoji(&name)
                .await?
                .iter()
                .any(|e| e.id == emoji.id)
        );
        assert_eq!(f.a.custom_emoji_by_name(&name).await?.id, emoji.id);
        assert_eq!(f.a.custom_emoji_by_names(vec![name]).await?[0].id, emoji.id);
        assert_eq!(
            f.a.fetch(&f.a.custom_emoji_url(&emoji.id)).await?.status,
            200
        );
    }
    f.wire.disconnect();
    event(&mut events, "disconnect", |e| {
        matches!(e, Event::Disconnected)
    })
    .await;
    let offline = f.post(&f.b, "posted during socket outage").await?;
    f.wire.reconnect();
    event(&mut events, "hello after reconnect", |e| {
        matches!(e, Event::Connected)
    })
    .await;
    assert!(
        f.a.posts_since(&f.public.id, offline.create_at - 1)
            .await?
            .posts
            .contains_key(&offline.id)
    );
    let after = f.post(&f.b, "posted after reconnect").await?;
    event(
        &mut events,
        "posted after reconnect",
        |e| matches!(e,Event::Posted{post,..} if post.id==after.id),
    )
    .await;
    Ok(())
}

#[tokio::test]
#[ignore = "requires MTC_LIVE_SERVER; modifies a disposable Mattermost server"]
async fn channel_lifecycle_sidebar_and_raw_events() -> Result<()> {
    let Some(f) = Live::provision().await? else {
        return Ok(());
    };
    let (_ws, mut events) = connected(&f.a).await;
    let channel = Live::create_channel(&f.a, &f.team, "lifecycle", &f.suffix, "O").await?;
    event(&mut events,"channel_created",|e| matches!(e,Event::ChannelsChanged{name,team_id,..} if name=="channel_created" && team_id==&f.team.id)).await;
    let _: Value = raw(
        &f.a,
        "PUT",
        &format!("channels/{}/patch", channel.id),
        json!({"header":"live updated header"}),
    )
    .await?;
    event(
        &mut events,
        "channel_updated",
        |e| matches!(e,Event::ChannelsChanged{name,..} if name=="channel_updated"),
    )
    .await;
    // Exact v10.11.26 api4/channel.go route and Client4.UpdateChannelPrivacy:
    // PUT /channels/{id}/privacy, with a privacy field, not /convert.
    let converted: mattermost::Channel = raw(
        &f.a,
        "PUT",
        &format!("channels/{}/privacy", channel.id),
        json!({"privacy":"P"}),
    )
    .await?;
    assert_eq!(converted.id, channel.id);
    assert_eq!(converted.kind, mattermost::ChannelType::Private);
    event(
        &mut events,
        "channel_converted",
        |e| matches!(e,Event::ChannelsChanged{name,..} if name=="channel_converted"),
    )
    .await;
    let _: Value = raw(
        &f.a,
        "DELETE",
        &format!("channels/{}", channel.id),
        Value::Null,
    )
    .await?;
    event(
        &mut events,
        "channel_deleted",
        |e| matches!(e,Event::ChannelsChanged{name,..} if name=="channel_deleted"),
    )
    .await;
    let _: Value = raw(
        &f.admin,
        "POST",
        &format!("channels/{}/restore", channel.id),
        json!({}),
    )
    .await?;
    event(
        &mut events,
        "channel_restored",
        |e| matches!(e,Event::ChannelsChanged{name,..} if name=="channel_restored"),
    )
    .await;

    let path = format!(
        "users/{}/teams/{}/channels/categories",
        f.alice.id, f.team.id
    );
    let mut category:Value=raw(&f.a,"POST",&path,json!({"user_id":f.alice.id,"team_id":f.team.id,"type":"custom","display_name":"Live category","sorting":"manual","channel_ids":[]})).await?;
    event(
        &mut events,
        "unmodeled sidebar_category_created is passed through",
        |e| matches!(e,Event::Other{name,..} if name=="sidebar_category_created"),
    )
    .await;
    category["display_name"] = json!("Updated live category");
    let _: Value = raw(
        &f.a,
        "PUT",
        &format!(
            "{path}/{}",
            category["id"].as_str().expect("sidebar category id")
        ),
        category.clone(),
    )
    .await?;
    event(&mut events, "sidebar_category_updated", |e| {
        matches!(e, Event::SidebarCategoriesChanged)
    })
    .await;
    let other: mattermost::Team = raw(
        &f.admin,
        "POST",
        "teams",
        json!({"name":format!("other{}",f.suffix),"display_name":"Other live team","type":"O"}),
    )
    .await?;
    let _: Value = raw(
        &f.admin,
        "POST",
        &format!("teams/{}/members", other.id),
        json!({"team_id":other.id,"user_id":f.alice.id}),
    )
    .await?;
    event(&mut events,"added_to_team",|e|matches!(e,Event::ChannelsChanged{name,team_id,user_id} if name=="added_to_team" && team_id==&other.id && user_id==&f.alice.id)).await;
    let _: Value = raw(
        &f.a,
        "DELETE",
        &format!("teams/{}/members/{}", other.id, f.alice.id),
        Value::Null,
    )
    .await?;
    event(&mut events,"leave_team",|e|matches!(e,Event::ChannelsChanged{name,team_id,..} if name=="leave_team" && team_id==&other.id)).await;
    // Legacy `preference_changed` and `channel_viewed` have unit fixtures; a
    // current server may emit only the plural variants. Raw frames are retained.
    Ok(())
}
