package io.anonchat.app

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.util.Log
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.result.PickVisualMediaRequest
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.clickable
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.ChatBubbleOutline
import androidx.compose.material.icons.outlined.Settings
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshotFlow
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.unit.sp
import androidx.core.content.ContextCompat
import com.journeyapps.barcodescanner.ScanContract
import com.journeyapps.barcodescanner.ScanOptions
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

/**
 * UI 重构说明（相比 0.1 早期版本）：
 *
 * 旧版把身份、中继、邀请、添加、电池、网络设置、联系人**全部竖着堆在一页**，
 * 找任何东西都要滚半天——自动化测试时连我自己都把中继地址打进了昵称框。
 *
 * 新版按 IM 的惯例拆成两个 Tab：
 * - **聊天**（默认）：状态条 + 联系人列表，核心内容放最上面
 * - **设置**：身份、接收服务、电池白名单、中继、通话网络
 *
 * 另外两个决定：
 * - **收消息服务自动启动**（有身份就开），不再让用户手动点"开始收消息"——
 *   一个 IM 需要"手动点开始才能收消息"本身就是设计错误。设置页保留手动开关。
 * - 邀请码、添加联系人都收进对话框，主界面只留入口。
 */
class MainActivity : ComponentActivity() {

    companion object {
        /** 从来电通知点进来时带这个标记，直接展开通话界面 */
        const val EXTRA_SHOW_CALL = "show_call"
        /** 调试用：跑 WebRTC 自检，只打日志 */
        const val EXTRA_SELFTEST = "selftest"

        /** 当前（前台时）打开的聊天页对应的会话键，服务用它抑制同会话通知 */
        var openPeer by mutableStateOf<String?>(null)
            internal set
        /** App 是否在前台 */
        var resumed = false
            internal set
    }

    private val notiPerm = registerForActivityResult(
        ActivityResultContracts.RequestPermission()
    ) {}

    /** 待发起通话：权限拿到之后再真正开始 */
    private var pendingCall: Pair<String, Boolean>? = null

    private val callPerm = registerForActivityResult(
        ActivityResultContracts.RequestMultiplePermissions()
    ) { res ->
        val p = pendingCall ?: return@registerForActivityResult
        pendingCall = null
        if (res.values.all { it }) {
            CallManager.startCall(p.first, p.second)?.let { Log.w("AnonChat", "start call failed: $it") }
        } else {
            Log.w("AnonChat", "call permission denied")
        }
    }

    private fun startCall(peer: String, video: Boolean) {
        val need = buildList {
            add(Manifest.permission.RECORD_AUDIO)
            if (video) add(Manifest.permission.CAMERA)
        }
        val missing = need.filter {
            ContextCompat.checkSelfPermission(this, it) != PackageManager.PERMISSION_GRANTED
        }
        if (missing.isEmpty()) {
            CallManager.startCall(peer, video)?.let { Log.w("AnonChat", "start call failed: $it") }
            return
        }
        pendingCall = peer to video
        callPerm.launch(missing.toTypedArray())
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        Repo.init(this)
        CallManager.init(this)
        ensureNotiPermission()
        if (intent?.getBooleanExtra(EXTRA_SHOW_CALL, false) == true) {
            Log.i("AnonChat", "launched from incoming call notification")
        }
        if (intent?.getBooleanExtra(EXTRA_SELFTEST, false) == true) {
            WebRtcSelfTest.run(this)
        }

        setContent {
            // 跟随系统深色模式：之前写死 MaterialTheme 默认（浅色），
            // 深色壁纸下状态栏图标自动变白但界面还是一片刺眼的白
            val dark = androidx.compose.foundation.isSystemInDarkTheme()
            // 配色不再用 M3 默认调色板：默认 primaryContainer 是浅紫，
            // 而聊天页里每个自己发的消息都取这个色，一整屏都是紫的。
            val skin = if (dark) SkinDark else SkinLight
            MaterialTheme(colorScheme = skinColorScheme(skin)) {
                CompositionLocalProvider(LocalSkin provides skin) {
                    Surface(modifier = Modifier.fillMaxSize(), color = skin.bg) {
                        // Android 15 起强制边到边：不处理安全区的话，底部输入栏会被
                        // 系统导航栏压住，点"发送"实际点到导航键。
                        Box(modifier = Modifier.safeDrawingPadding()) {
                            Root(
                                onStartService = { startMessaging() },
                                onStopService = { stopMessaging() },
                                onStartCall = { peer, video -> startCall(peer, video) },
                            )
                            // 通话界面盖在最上层：它可能在聊天页、也可能在后台被通知拉起
                            CallScreen(onClose = {})
                        }
                    }
                }
            }
        }
    }

    private fun ensureNotiPermission() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) return
        if (ContextCompat.checkSelfPermission(this, Manifest.permission.POST_NOTIFICATIONS)
            == PackageManager.PERMISSION_GRANTED
        ) return
        notiPerm.launch(Manifest.permission.POST_NOTIFICATIONS)
    }

    override fun onResume() {
        super.onResume()
        resumed = true
    }

    override fun onPause() {
        super.onPause()
        resumed = false
    }

    private fun startMessaging() {
        val i = Intent(this, MessagingService::class.java).apply {
            action = MessagingService.ACTION_START
        }
        ContextCompat.startForegroundService(this, i)
    }

    private fun stopMessaging() {
        val i = Intent(this, MessagingService::class.java).apply {
            action = MessagingService.ACTION_STOP
        }
        startService(i)
    }
}

// ---------------------------------------------------------------- 路由

private enum class Tab(@androidx.annotation.StringRes val label: Int) {
    Chats(R.string.tab_chats),
    Settings(R.string.tab_settings),
}

private sealed interface Screen {
    data object Setup : Screen
    data class Chat(val peer: String) : Screen
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun Root(
    onStartService: () -> Unit,
    onStopService: () -> Unit,
    onStartCall: (String, Boolean) -> Unit,
) {
    val hasIdentity = Repo.hasIdentity()
    var screen by remember {
        mutableStateOf<Any>(if (hasIdentity) Tab.Chats else Screen.Setup)
    }
    var tick by remember { mutableStateOf(0) }

    // 底部 Tab 未读角标：读 inboxTick（Compose 可观察状态）让新消息驱动重组
    @Suppress("UNUSED_EXPRESSION")
    MessagingService.inboxTick
    val unreadTotal = Repo.totalUnread()

    // 有身份就自动开始收消息：IM 不应该要求用户手动"上线"
    LaunchedEffect(hasIdentity) {
        if (hasIdentity) onStartService()
    }

    when (val s = screen) {
        Screen.Setup -> SetupScreen(onDone = { screen = Tab.Chats; tick++ })

        is Tab -> Scaffold(
            topBar = {
                TopAppBar(
                    title = { Text(stringResource(R.string.app_name)) },
                    colors = TopAppBarDefaults.topAppBarColors(
                        containerColor = MaterialTheme.colorScheme.surface
                    )
                )
            },
            bottomBar = {
                NavigationBar {
                    Tab.entries.forEach { t ->
                        NavigationBarItem(
                            selected = s == t,
                            onClick = { screen = t },
                            label = { Text(stringResource(t.label)) },
                            // 图标用真图标；之前这里也放文字，和 label 重复出现两行"聊天"
                            icon = {
                                val ic: @Composable () -> Unit = {
                                    androidx.compose.material3.Icon(
                                        if (t == Tab.Chats) Icons.Outlined.ChatBubbleOutline
                                        else Icons.Outlined.Settings,
                                        contentDescription = stringResource(t.label)
                                    )
                                }
                                if (t == Tab.Chats && unreadTotal > 0) {
                                    androidx.compose.material3.BadgedBox(
                                        badge = {
                                            androidx.compose.material3.Badge {
                                                Text(if (unreadTotal > 99) "99+" else unreadTotal.toString())
                                            }
                                        }
                                    ) { ic() }
                                } else ic()
                            }
                        )
                    }
                }
            }
        ) { pad ->
            Box(Modifier.padding(pad)) {
                when (s) {
                    Tab.Chats -> ChatsTab(
                        tick = tick,
                        onOpenChat = { screen = Screen.Chat(it) },
                        onChanged = { tick++ },
                        onStartService = onStartService,
                    )
                    Tab.Settings -> SettingsTab(
                        tick = tick,
                        onStartService = onStartService,
                        onStopService = onStopService,
                        onChanged = { tick++ },
                    )
                    else -> {}
                }
            }
        }

        is Screen.Chat -> ChatScreen(
            peer = s.peer,
            onBack = { screen = Tab.Chats; tick++ },
            onStartCall = onStartCall,
            // 陌生人加为联系人后会话改名，直接切到新名字的那个会话，
            // 用户刚打的那段对话（已迁移）就在眼前，不用回列表再点一次
            onPeerRenamed = { screen = Screen.Chat(it); tick++ },
        )
    }
}

// ---------------------------------------------------------------- 首次设置

/**
 * 首次启动引导，三步（对应 docs 第 9.2 节的流程）：
 *
 * ① 身份：起名 + 创建（或从恢复短语恢复）——密钥本地生成，无注册；
 * ② 抄写恢复短语：新建身份后强制过一遍。这串词是身份的唯一备份，
 *    12 个词网格展示，必须勾"我已抄写"才能继续；
 * ③ 选中继：官方不运营服务器，新用户最大的困惑是"连谁"。
 *    列表自动测速排序，同时永远保留"手输自己的中继"的入口。
 */
private sealed interface SetupStep {
    data object Identity : SetupStep
    data object Phrase : SetupStep
    data object Relay : SetupStep
}

@Composable
private fun SetupScreen(onDone: () -> Unit) {
    var step by remember { mutableStateOf<SetupStep>(SetupStep.Identity) }
    var phrase by remember { mutableStateOf<String?>(null) }

    when (step) {
        SetupStep.Identity -> IdentityStep(
            onCreated = { phrase = it; step = SetupStep.Phrase },
            onRestored = { step = SetupStep.Relay },
        )
        SetupStep.Phrase -> PhraseStep(
            phrase = phrase ?: "",
            onConfirmed = { step = SetupStep.Relay },
        )
        SetupStep.Relay -> RelayStep(onDone = onDone)
    }
}

@Composable
private fun IdentityStep(
    onCreated: (phrase: String) -> Unit,
    onRestored: () -> Unit,
) {
    var name by remember { mutableStateOf("") }
    var phraseInput by remember { mutableStateOf("") }
    var restoreMode by remember { mutableStateOf(false) }
    var err by remember { mutableStateOf<String?>(null) }
    var busy by remember { mutableStateOf(false) }
    val ctx = androidx.compose.ui.platform.LocalContext.current

    Column(
        Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(24.dp),
        verticalArrangement = Arrangement.spacedBy(16.dp)
    ) {
        Spacer(Modifier.height(24.dp))
        Text(stringResource(R.string.app_name), style = MaterialTheme.typography.headlineSmall)
        Text(
            stringResource(R.string.setup_tagline),
            style = MaterialTheme.typography.bodySmall
        )

        OutlinedTextField(
            value = name, onValueChange = { name = it },
            label = { Text(stringResource(R.string.setup_nickname_label)) },
            singleLine = true, modifier = Modifier.fillMaxWidth()
        )

        if (restoreMode) {
            OutlinedTextField(
                value = phraseInput, onValueChange = { phraseInput = it },
                label = { Text(stringResource(R.string.setup_phrase_label)) },
                minLines = 2, maxLines = 4,
                textStyle = androidx.compose.ui.text.TextStyle(
                    fontSize = 13.sp, fontFamily = FontFamily.Monospace
                ),
                modifier = Modifier.fillMaxWidth()
            )
            Text(
                stringResource(R.string.setup_restore_hint),
                style = MaterialTheme.typography.bodySmall
            )
        }

        err?.let { Text(it, color = MaterialTheme.colorScheme.error) }

        if (!restoreMode) {
            Button(
                enabled = !busy && name.isNotBlank(),
                onClick = {
                    busy = true; err = null
                    kotlinx.coroutines.MainScope().launch {
                        val r = withContext(Dispatchers.IO) { Repo.createIdentity(name.trim()) }
                        if (r.ok) {
                            // 立刻把恢复短语取出来给下一步展示
                            val p = withContext(Dispatchers.IO) { Repo.backupPhrase() }
                            if (p.ok && p.data != null) onCreated(p.data)
                            else err = errText(ctx, p)
                        } else err = errText(ctx, r)
                        busy = false
                    }
                },
                modifier = Modifier.fillMaxWidth().height(52.dp)
            ) { Text(if (busy) stringResource(R.string.setup_creating) else stringResource(R.string.setup_create)) }
            TextButton(
                onClick = { restoreMode = true },
                modifier = Modifier.fillMaxWidth()
            ) { Text(stringResource(R.string.setup_have_phrase)) }
        } else {
            Button(
                enabled = !busy && name.isNotBlank() && phraseInput.isNotBlank(),
                onClick = {
                    busy = true; err = null
                    kotlinx.coroutines.MainScope().launch {
                        val r = withContext(Dispatchers.IO) {
                            Repo.restore(phraseInput.trim(), name.trim())
                        }
                        if (r.ok) onRestored() else err = errText(ctx, r)
                        busy = false
                    }
                },
                modifier = Modifier.fillMaxWidth().height(52.dp)
            ) { Text(if (busy) stringResource(R.string.setup_restoring) else stringResource(R.string.setup_restore)) }
            TextButton(
                onClick = { restoreMode = false },
                modifier = Modifier.fillMaxWidth()
            ) { Text(stringResource(R.string.setup_back_to_create)) }
        }
    }
}

/** 12 个词 3 列 4 行网格，带序号，抄写方便 */
@Composable
private fun PhraseStep(phrase: String, onConfirmed: () -> Unit) {
    var checked by remember { mutableStateOf(false) }
    val words = remember(phrase) { phrase.split(Regex("\\s+")).filter { it.isNotBlank() } }

    Column(
        Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(24.dp),
        verticalArrangement = Arrangement.spacedBy(16.dp)
    ) {
        Spacer(Modifier.height(24.dp))
        Text(stringResource(R.string.phrase_title), style = MaterialTheme.typography.headlineSmall)
        Text(
            stringResource(R.string.phrase_hint),
            style = MaterialTheme.typography.bodySmall
        )

        Card(Modifier.fillMaxWidth()) {
            Column(Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                words.chunked(3).forEachIndexed { rowIdx, rowWords ->
                    Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceEvenly) {
                        rowWords.forEachIndexed { colIdx, w ->
                            Text(
                                "${rowIdx * 3 + colIdx + 1}. $w",
                                fontFamily = FontFamily.Monospace,
                                fontSize = 15.sp,
                                fontWeight = FontWeight.Medium
                            )
                        }
                    }
                }
            }
        }
        Text(
            stringResource(R.string.phrase_warning),
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.error
        )

        Row(verticalAlignment = Alignment.CenterVertically) {
            androidx.compose.material3.Checkbox(checked = checked, onCheckedChange = { checked = it })
            Text(stringResource(R.string.phrase_confirmed))
        }

        Button(
            enabled = checked,
            onClick = onConfirmed,
            modifier = Modifier.fillMaxWidth().height(52.dp)
        ) { Text(stringResource(R.string.phrase_continue)) }
    }
}

/** ③ 选中继：列表（自动测速）+ 自定义输入，两扇门都开着 */
@Composable
private fun RelayStep(onDone: () -> Unit) {
    val ctx = androidx.compose.ui.platform.LocalContext.current
    var list by remember { mutableStateOf<List<RelayEntry>>(emptyList()) }
    var source by remember { mutableStateOf("") }
    var latency by remember { mutableStateOf<Map<String, Long>>(emptyMap()) }
    var custom by remember { mutableStateOf("") }
    var loading by remember { mutableStateOf(true) }
    var busyWith by remember { mutableStateOf<String?>(null) }
    // DHT 自动发现：null = 还没点过；空集 = 点过但没找到
    var discovering by remember { mutableStateOf(false) }
    var dhtFound by remember { mutableStateOf<Int?>(null) }
    var err by remember { mutableStateOf<String?>(null) }

    LaunchedEffect(Unit) {
        val (l, s) = withContext(Dispatchers.IO) { RelayList.load(ctx) }
        list = l
        source = s
        loading = false
        // 测速放后台，先让用户看到列表再排序
        if (l.isNotEmpty()) {
            val m = RelayList.probeAll(l.map { it.url })
            latency = m
        }
    }

    fun use(url: String) {
        busyWith = url
        err = null
        kotlinx.coroutines.MainScope().launch {
            val r = Repo.setRelay(ctx, url.trim())
            busyWith = null
            // 失败必须拦住：`setRelay` 会顺手建收件队列，它失败就等于
            // "这台设备没有收件地址"。此前这里忽略返回值直接 onDone()，
            // 用户带着一个空队列进主界面，之后发出的每条消息都不带 reply_qid，
            // 对方看到的是"没有回信线索，无法回复"，还以为是版本问题。
            if (r.ok) {
                onDone()
            } else {
                err = errText(ctx, r)
            }
        }
    }

    Column(
        Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(24.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp)
    ) {
        Text(stringResource(R.string.relay_title), style = MaterialTheme.typography.headlineSmall)
        Text(
            stringResource(R.string.relay_hint),
            style = MaterialTheme.typography.bodySmall
        )
        err?.let {
            Text(it, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.error)
        }

        when {
            loading -> Text(stringResource(R.string.relay_loading))
            else -> {
                // DHT 自动发现：一条都不用手输。找到的地址会并进下面的列表一起测速。
                Button(
                    enabled = busyWith == null && !discovering,
                    onClick = {
                        discovering = true
                        kotlinx.coroutines.MainScope().launch {
                            val r = withContext(Dispatchers.IO) { Repo.discoverRelays() }
                            val got = r.data.orEmpty()
                            if (got.isNotEmpty()) {
                                val known = list.map { it.url }.toSet()
                                val added = got.filter { it !in known }
                                    .map { RelayEntry(it, "", "") }
                                list = list + added
                                // 只测新来的，避免把已有结果覆盖掉
                                val m = RelayList.probeAll(added.map { it.url })
                                latency = latency + m
                            }
                            dhtFound = got.size
                            discovering = false
                        }
                    },
                    modifier = Modifier.fillMaxWidth()
                ) {
                    Text(
                        when {
                            discovering -> stringResource(R.string.relay_dht_discovering)
                            else -> stringResource(R.string.relay_dht_discover)
                        }
                    )
                }
                when {
                    discovering -> Text(
                        stringResource(R.string.relay_dht_hint),
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant
                    )
                    dhtFound == 0 -> Text(
                        stringResource(R.string.relay_dht_none),
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.error
                    )
                    dhtFound != null && dhtFound!! > 0 -> Text(
                        stringResource(R.string.relay_dht_found, dhtFound!!),
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.primary
                    )
                }
            }
        }

        when {
            loading -> {}
            list.isEmpty() -> Card(Modifier.fillMaxWidth()) {
                Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                    Text(stringResource(R.string.relay_empty_title), fontWeight = FontWeight.Medium)
                    Text(
                        stringResource(R.string.relay_empty_hint),
                        style = MaterialTheme.typography.bodySmall
                    )
                }
            }
            else -> {
                Text(
                    source,
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant
                )
                // 有测速结果就按延迟排，没结果的排后面
                val sorted = if (latency.isEmpty()) list else list.sortedBy { e ->
                    latency[e.url]?.takeIf { it >= 0 } ?: Long.MAX_VALUE
                }
                sorted.forEach { e ->
                    val ms = latency[e.url]
                    Card(
                        Modifier.fillMaxWidth().clickable(enabled = busyWith == null) { use(e.url) }
                    ) {
                        Row(
                            Modifier.padding(12.dp),
                            verticalAlignment = Alignment.CenterVertically
                        ) {
                            Column(Modifier.weight(1f)) {
                                Text(e.url, fontSize = 12.sp, fontFamily = FontFamily.Monospace)
                                Text(
                                    e.region.ifBlank { stringResource(R.string.relay_region_unknown) },
                                    style = MaterialTheme.typography.labelSmall,
                                    color = MaterialTheme.colorScheme.onSurfaceVariant
                                )
                            }
                            Text(
                                when {
                                    ms == null -> stringResource(R.string.relay_probing)
                                    ms < 0 -> stringResource(R.string.relay_unreachable)
                                    else -> "${ms}ms"
                                },
                                style = MaterialTheme.typography.labelMedium,
                                color = when {
                                    ms == null -> MaterialTheme.colorScheme.onSurfaceVariant
                                    ms < 0 -> MaterialTheme.colorScheme.error
                                    else -> MaterialTheme.colorScheme.primary
                                }
                            )
                        }
                    }
                }
            }
        }

        HorizontalDivider()
        Text(stringResource(R.string.relay_custom_title), style = MaterialTheme.typography.titleSmall)
        OutlinedTextField(
            value = custom, onValueChange = { custom = it },
            label = { Text(stringResource(R.string.relay_custom_label)) },
            singleLine = true,
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri),
            modifier = Modifier.fillMaxWidth()
        )
        Button(
            enabled = busyWith == null && custom.isNotBlank(),
            onClick = { use(custom) },
            modifier = Modifier.fillMaxWidth().height(48.dp)
        ) {
                Text(
                    if (busyWith != null) stringResource(R.string.relay_connecting)
                    else stringResource(R.string.relay_use)
                )
            }
    }
}

// ---------------------------------------------------------------- 聊天 Tab

@Composable
private fun ChatsTab(
    tick: Int,
    onOpenChat: (String) -> Unit,
    onChanged: () -> Unit,
    onStartService: () -> Unit,
) {
    val ctx = androidx.compose.ui.platform.LocalContext.current

    var contacts by remember { mutableStateOf(listOf<Contact>()) }
    var unverified by remember { mutableStateOf(listOf<String>()) }
    // 未读数与最后一条消息必须作为状态随刷新一起更新：
    // LazyColumn 的 items 内容 lambda 在列表 equals 相等时不会重新执行，
    // 直接在 item 里调 Repo.unreadOf() 会永远显示旧值（角标卡死的根因）
    var unreadMap by remember { mutableStateOf(mapOf<String, Int>()) }
    // 存结构化的预览而不是拼好的字符串：文案要随系统语言变，
    // 在列表里用 stringResource 现算（见 previewText）
    var lastMsgs by remember { mutableStateOf(mapOf<String, Preview?>()) }
    var showAdd by remember { mutableStateOf(false) }
    var showInvite by remember { mutableStateOf(false) }
    var addCode by remember { mutableStateOf("") }
    var inviteCode by remember { mutableStateOf<String?>(null) }
    var busy by remember { mutableStateOf(false) }
    var msg by remember { mutableStateOf<String?>(null) }

    // 长按管理：备注名 / 删聊天记录 / 删联系人
    var manageKey by remember { mutableStateOf<String?>(null) }
    var aliasTarget by remember { mutableStateOf<Contact?>(null) }
    var aliasText by remember { mutableStateOf("") }
    /** (会话键, 是否连同联系人一起删) */
    var delTarget by remember { mutableStateOf<Pair<String, Boolean>?>(null) }

    // 实时刷新：tick 是本页操作触发的手动刷新，inboxTick 是服务收到新消息的信号。
    // 之前只有 tick，消息到了界面一动不动，要切 Tab 才能看到。
    LaunchedEffect(tick, MessagingService.inboxTick) {
        withContext(Dispatchers.IO) {
            contacts = Repo.contacts()
            unverified = Repo.unverifiedConversations()
            // 会话键是 dh（0.12.0 起），显示名单独查——备注名改了不会裂会话
            unreadMap = contacts.associate { it.dh to Repo.unreadOf(it.dh) } +
                unverified.associate { it to Repo.unreadOf(it) }
            lastMsgs = contacts.associate { c ->
                // 只要最后一条，不再为了一行预览把整个会话拷贝出来
                c.dh to Repo.lastMsgOf(c.dh)?.preview
            }
        }
    }
    // 阅后即焚到期会改变预览/未读，但没有 inboxTick 信号（没新消息到达）。
    // 会话里可能有焚毁消息时每秒轻扫一次，把已烧掉的预览同步掉（onChanged
    // 触发外层 tick++ 重组）；没有焚毁消息时 sweepExpired 也是一次内存遍历，
    // 会话数为几十的量级，代价可忽略。
    LaunchedEffect(tick) {
        while (true) {
            delay(1_000)
            val n = withContext(Dispatchers.IO) { Repo.sweepExpired() }
            if (n > 0) onChanged()
        }
    }

    Column(Modifier.fillMaxSize().padding(16.dp)) {
        // ---- 状态条：服务没跑、电池没授权，都比"联系人"更需要先被看到
        val running = MessagingService.state.running
        val whitelisted = BatteryOpt.isIgnoring(ctx)
        if (!running) {
            StatusBanner(
                text = stringResource(R.string.chats_banner_service_off),
                action = stringResource(R.string.chats_banner_start),
                container = MaterialTheme.colorScheme.errorContainer
            ) { onStartService() }
        } else if (!whitelisted) {
            StatusBanner(
                text = stringResource(R.string.chats_banner_battery),
                action = stringResource(R.string.chats_banner_grant),
                container = MaterialTheme.colorScheme.secondaryContainer
            ) {
                BatteryOpt.requestIgnore(ctx)
                onChanged()
            }
        }

        // ---- 联系人
        Row(
            Modifier.fillMaxWidth().padding(top = 8.dp),
            verticalAlignment = Alignment.CenterVertically
        ) {
            Text(stringResource(R.string.chats_contacts), style = MaterialTheme.typography.titleMedium, modifier = Modifier.weight(1f))
            TextButton(onClick = { showInvite = true }) { Text(stringResource(R.string.chats_my_invite)) }
        }

        LazyColumn(Modifier.fillMaxSize(), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            if (contacts.isEmpty() && unverified.isEmpty()) {
                item {
                    Card(Modifier.fillMaxWidth()) {
                        Column(Modifier.padding(20.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                            Text(stringResource(R.string.chats_empty_title), fontWeight = FontWeight.Medium)
                            Text(
                                stringResource(R.string.chats_empty_hint),
                                style = MaterialTheme.typography.bodySmall
                            )
                        }
                    }
                }
            }

            if (unverified.isNotEmpty()) {
                item {
                    Text(
                        stringResource(R.string.chats_unverified_header),
                        style = MaterialTheme.typography.labelMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant
                    )
                }
                items(unverified) { key ->
                    ContactRow(
                        name = displayNameOf(key),
                        sub = if (Repo.cardOf(key) != null) stringResource(R.string.chats_unverified_with_card)
                        else stringResource(R.string.chats_unverified_plain),
                        unread = unreadMap[key] ?: 0,
                        onClick = { onOpenChat(key) },
                        onLongClick = { manageKey = key },
                    )
                }
            }

            items(contacts) { c ->
                ContactRow(
                    name = c.display,
                    sub = lastMsgs[c.dh]?.let { previewText(it) }
                        ?: stringResource(R.string.chats_no_messages),
                    highlight = lastMsgs[c.dh] != null,
                    unread = unreadMap[c.dh] ?: 0,
                    onClick = { onOpenChat(c.dh) },
                    onLongClick = { manageKey = c.dh },
                )
            }

            item {
                OutlinedButton(
                    onClick = { showAdd = true },
                    modifier = Modifier.fillMaxWidth()
                ) { Text(stringResource(R.string.chats_add_contact)) }
            }
            item { Spacer(Modifier.height(12.dp)) }
        }
    }

    // ---- 长按管理：备注名 / 删记录 / 删联系人
    manageKey?.let { key ->
        val c = contacts.firstOrNull { it.dh == key }
        AlertDialog(
            onDismissRequest = { manageKey = null },
            title = { Text(c?.display ?: displayNameOf(key)) },
            text = {
                Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
                    if (c != null) {
                        MenuRow(stringResource(R.string.chats_manage_alias)) {
                            manageKey = null
                            aliasText = c.alias
                            aliasTarget = c
                        }
                    }
                    MenuRow(stringResource(R.string.chats_manage_delete_chat)) {
                        manageKey = null
                        delTarget = key to false
                    }
                    if (c != null) {
                        MenuRow(stringResource(R.string.chats_manage_delete_contact)) {
                            manageKey = null
                            delTarget = key to true
                        }
                    }
                }
            },
            confirmButton = { TextButton(onClick = { manageKey = null }) { Text(stringResource(R.string.action_cancel)) } }
        )
    }

    aliasTarget?.let { c ->
        AlertDialog(
            onDismissRequest = { aliasTarget = null },
            title = { Text(stringResource(R.string.alias_title)) },
            text = {
                Column {
                    OutlinedTextField(
                        value = aliasText,
                        onValueChange = { aliasText = it },
                        label = { Text(stringResource(R.string.alias_label)) },
                        singleLine = true,
                        modifier = Modifier.fillMaxWidth()
                    )
                    Spacer(Modifier.height(6.dp))
                    Text(
                        stringResource(R.string.alias_hint),
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant
                    )
                }
            },
            confirmButton = {
                TextButton(onClick = {
                    val alias = aliasText
                    aliasTarget = null
                    kotlinx.coroutines.MainScope().launch {
                        val r = withContext(Dispatchers.IO) { Repo.setAlias(c.dh, alias) }
                        if (r.ok) {
                            msg = ctx.getString(R.string.alias_saved)
                            onChanged()
                        } else msg = errText(ctx, r)
                    }
                }) { Text(stringResource(R.string.action_save)) }
            },
            dismissButton = {
                TextButton(onClick = { aliasTarget = null }) { Text(stringResource(R.string.action_cancel)) }
            }
        )
    }

    delTarget?.let { (key, withContact) ->
        val label = displayNameOf(key)
        AlertDialog(
            onDismissRequest = { delTarget = null },
            title = {
                Text(
                    if (withContact) stringResource(R.string.delete_contact_title)
                    else stringResource(R.string.delete_chat_title)
                )
            },
            text = {
                Text(
                    if (withContact) stringResource(R.string.delete_contact_body, label)
                    else stringResource(R.string.delete_chat_body, label)
                )
            },
            confirmButton = {
                TextButton(onClick = {
                    delTarget = null
                    kotlinx.coroutines.MainScope().launch {
                        val r = withContext(Dispatchers.IO) {
                            if (withContact) Repo.deleteContact(key)
                            else Res(Repo.deleteConversation(key), null)
                        }
                        msg = if (r.ok) ctx.getString(R.string.delete_done) else errText(ctx, r)
                        onChanged()
                    }
                }) { Text(stringResource(R.string.action_delete)) }
            },
            dismissButton = {
                TextButton(onClick = { delTarget = null }) { Text(stringResource(R.string.action_cancel)) }
            }
        )
    }

    // 提示尽量内联：叠在对话框上的 AlertDialog 会把下层内容挡住
    if (msg != null && !showInvite && !showAdd) {
        AlertDialog(
            onDismissRequest = { msg = null },
            title = { Text(stringResource(R.string.dialog_notice)) },
            text = { Text(msg!!) },
            confirmButton = { TextButton(onClick = { msg = null }) { Text(stringResource(R.string.action_ok)) } }
        )
    }

    // ---- 我的邀请码
    if (showInvite) {
        // 错误必须在对话框**内部**显示：之前写进 msg，而 msg 的提示对话框
        // 在 showInvite 开着时被抑制 —— 生成失败时用户看不到任何反馈，
        // 看起来就像"按钮点了没反应"。
        var inviteErr by remember { mutableStateOf<String?>(null) }
        AlertDialog(
            onDismissRequest = { showInvite = false },
            title = { Text(stringResource(R.string.invite_title)) },
            text = {
                Column {
                    Text(
                        stringResource(R.string.invite_hint),
                        style = MaterialTheme.typography.bodySmall
                    )
                    Spacer(Modifier.height(8.dp))
                    val clip = LocalClipboardManager.current
                    OutlinedButton(
                        onClick = {
                            kotlinx.coroutines.MainScope().launch {
                                val r = withContext(Dispatchers.IO) { Repo.invite() }
                                if (r.ok && r.data != null) {
                                    inviteCode = r.data
                                    inviteErr = null
                                    clip.setText(AnnotatedString(r.data))
                                } else {
                                    inviteErr = errText(ctx, r)
                                }
                            }
                        },
                        modifier = Modifier.fillMaxWidth()
                    ) { Text(if (inviteCode == null) stringResource(R.string.invite_generate) else stringResource(R.string.invite_regenerate)) }
                    inviteErr?.let {
                        Spacer(Modifier.height(6.dp))
                        Text(
                            it,
                            color = MaterialTheme.colorScheme.error,
                            style = MaterialTheme.typography.bodySmall
                        )
                    }
                    inviteCode?.let { code ->
                        Spacer(Modifier.height(8.dp))
                        Text(
                            stringResource(R.string.invite_copied),
                            style = MaterialTheme.typography.labelSmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant
                        )
                        Spacer(Modifier.height(8.dp))
                        // 二维码：面对面加好友不用传 353 字符的字符串了
                        val qr = remember(code) { runCatching { qrBitmap(code, 512) }.getOrNull() }
                        if (qr != null) {
                            androidx.compose.foundation.Image(
                                bitmap = qr.asImageBitmap(),
                                contentDescription = stringResource(R.string.invite_qr_desc),
                                modifier = Modifier
                                    .size(240.dp)
                                    .align(Alignment.CenterHorizontally)
                            )
                            Text(
                                stringResource(R.string.invite_qr_hint),
                                style = MaterialTheme.typography.labelSmall,
                                color = MaterialTheme.colorScheme.onSurfaceVariant,
                                modifier = Modifier.fillMaxWidth(),
                                textAlign = TextAlign.Center
                            )
                        }
                        Spacer(Modifier.height(4.dp))
                        Text(
                            code,
                            fontSize = 10.sp,
                            fontFamily = FontFamily.Monospace,
                            maxLines = 6,
                            overflow = TextOverflow.Ellipsis,
                            modifier = Modifier
                                .fillMaxWidth()
                                .background(
                                    MaterialTheme.colorScheme.surfaceVariant,
                                    RoundedCornerShape(8.dp)
                                )
                                .padding(8.dp)
                        )
                    }
                }
            },
            confirmButton = { TextButton(onClick = { showInvite = false }) { Text(stringResource(R.string.action_done)) } }
        )
    }

    // ---- 添加联系人
    // 扫码：结果直接填进输入框，用户确认后再点"添加"（不自动添加，防止误扫）
    val scanLauncher = rememberLauncherForActivityResult(
        ScanContract()
    ) { result ->
        result.contents?.let { addCode = it.trim() }
    }

    if (showAdd) {
        AlertDialog(
            onDismissRequest = { showAdd = false },
            title = { Text(stringResource(R.string.add_title)) },
            text = {
                Column {
                    Text(stringResource(R.string.add_hint), style = MaterialTheme.typography.bodySmall)
                    Spacer(Modifier.height(8.dp))
                    OutlinedTextField(
                        value = addCode, onValueChange = { addCode = it },
                        minLines = 3, maxLines = 3,
                        textStyle = androidx.compose.ui.text.TextStyle(
                            fontSize = 11.sp, fontFamily = FontFamily.Monospace
                        ),
                        modifier = Modifier.fillMaxWidth()
                    )
                    Spacer(Modifier.height(8.dp))
                    OutlinedButton(
                        onClick = { scanLauncher.launch(ScanOptions().apply {
                            setPrompt(ctx.getString(R.string.add_scan_prompt))
                            setBeepEnabled(false)
                            // 用我们自己的扫码页：库默认的 CaptureActivity 被声明成 sensorLandscape，
                            // 一进去就横屏，而且全屏无标题栏、没有任何退出按钮
                            setCaptureActivity(QrScanActivity::class.java)
                            // 关掉"锁定当前方向"：库按物理旋转决定，横着拿手机时会覆盖掉我们的竖屏声明
                            setOrientationLocked(false)
                        }) },
                        modifier = Modifier.fillMaxWidth()
                    ) { Text(stringResource(R.string.add_scan)) }
                }
            },
            confirmButton = {
                Button(
                    enabled = addCode.isNotBlank() && !busy,
                    onClick = {
                        busy = true; msg = null
                        kotlinx.coroutines.MainScope().launch {
                            val r = withContext(Dispatchers.IO) { Repo.add(addCode.trim(), null) }
                            if (r.ok) {
                                addCode = ""
                                showAdd = false
                                contacts = withContext(Dispatchers.IO) { Repo.contacts() }
                                onChanged()
                            } else msg = errText(ctx, r)
                            busy = false
                        }
                    }
                ) { Text(if (busy) stringResource(R.string.add_adding) else stringResource(R.string.add_action)) }
            },
            dismissButton = { TextButton(onClick = { showAdd = false }) { Text(stringResource(R.string.action_cancel)) } }
        )
    }
}

@Composable
private fun StatusBanner(
    text: String,
    action: String,
    container: Color,
    onClick: () -> Unit,
) {
    Card(
        Modifier.fillMaxWidth(),
        colors = CardDefaults.cardColors(containerColor = container)
    ) {
        Row(
            Modifier.padding(horizontal = 12.dp, vertical = 8.dp),
            verticalAlignment = Alignment.CenterVertically
        ) {
            Text(text, style = MaterialTheme.typography.bodySmall, modifier = Modifier.weight(1f))
            TextButton(onClick = onClick) { Text(action) }
        }
    }
}

@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun ContactRow(
    name: String,
    sub: String,
    highlight: Boolean = false,
    unread: Int = 0,
    onClick: () -> Unit,
    /** 长按打开管理菜单（备注名 / 删记录 / 删联系人） */
    onLongClick: (() -> Unit)? = null,
) {
    Card(
        Modifier.fillMaxWidth().combinedClickable(
            onClick = onClick,
            onLongClick = onLongClick,
        )
    ) {
        Row(
            Modifier.padding(12.dp),
            verticalAlignment = Alignment.CenterVertically
        ) {
            // 确定性头像：名字哈希 → 色相 + 首字，零存储。
            // 之前全列表一个 primaryContainer 灰底，两个名字同首字就分不清
            Avatar(name, 42.dp)
            Spacer(Modifier.width(12.dp))
            Column(Modifier.weight(1f)) {
                Text(name, fontWeight = FontWeight.Medium, maxLines = 1, overflow = TextOverflow.Ellipsis)
                Text(
                    sub,
                    style = MaterialTheme.typography.bodySmall,
                    color = if (highlight) MaterialTheme.colorScheme.onSurface
                    else MaterialTheme.colorScheme.onSurfaceVariant,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis
                )
            }
            // 未读角标
            if (unread > 0) {
                androidx.compose.material3.Badge {
                    Text(if (unread > 99) "99+" else unread.toString())
                }
            }
        }
    }
}

/**
 * 会话键 → 界面显示名（本地化版）。
 *
 * [Repo.displayName] 给的是"陌生人 abc12345"这种带中文前缀的名字——
 * 那个前缀是存储语义（见 [Repo.STRANGER_PREFIX]），不能动。这里只把
 * **显示**的那一段换成当前语言，存的东西原样不动。
 */
@Composable
private fun displayNameOf(key: String): String {
    val raw = Repo.displayName(key)
    return if (raw.startsWith(Repo.STRANGER_PREFIX)) {
        stringResource(R.string.stranger_prefix, raw.removePrefix(Repo.STRANGER_PREFIX))
    } else raw
}

/**
 * 会话列表里那一行预览的文案。
 *
 * 数据层给的是结构（[Preview]），这里按当前语言拼成句子——
 * 和第 4 批 IceProbe 的做法一致：接口只给类型和参数，界面负责措辞。
 */
@Composable
private fun previewText(p: Preview): String {
    val base = when (p) {
        is Preview.Text -> p.text
        is Preview.File ->
            if (p.name.isBlank()) stringResource(R.string.preview_file_generic)
            else stringResource(R.string.preview_file_named, p.name)
        is Preview.Voice -> stringResource(R.string.preview_voice, Voice.bubbleLabel(p.durMs))
        is Preview.Image -> stringResource(R.string.preview_image)
        is Preview.Video -> stringResource(R.string.preview_video)
        is Preview.Recall ->
            stringResource(
                if (p.byMe) R.string.recall_note_mine else R.string.recall_note_peer
            )
        // 与聊天页气泡走同一套资源，避免同一件事在列表和气泡里说法不一样
        is Preview.Call -> callPreviewText(p)
    }
    return if (p.failed) stringResource(R.string.preview_failed, base) else base
}

/**
 * 多选模式的顶栏：左边退出，中间显示已选数量。
 * 顶栏和输入栏都要换掉，否则用户没有"退出多选"的出口。
 */
@Composable
private fun SelectionBar(count: Int, onCancel: () -> Unit) {
    val skin = LocalSkin.current
    Row(
        Modifier.fillMaxWidth().height(SkinDims.barHeight).background(skin.bar),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(
            Modifier.size(SkinDims.barHeight).clickable(onClick = onCancel),
            contentAlignment = Alignment.Center,
        ) {
            Text("✕", fontSize = 20.sp, color = skin.icon)
        }
        Text(
            if (count > 0) stringResource(R.string.sel_title_n, count)
            else stringResource(R.string.sel_title_none),
            fontSize = 16.sp,
            fontWeight = FontWeight.Medium,
            color = skin.icon,
            modifier = Modifier.weight(1f),
        )
        Spacer(Modifier.width(12.dp))
    }
}

// ---------------------------------------------------------------- 设置 Tab
@Composable
private fun SettingsTab(
    tick: Int,
    onStartService: () -> Unit,
    onStopService: () -> Unit,
    onChanged: () -> Unit,
) {
    val ctx = androidx.compose.ui.platform.LocalContext.current
    val clip = LocalClipboardManager.current

    var myId by remember { mutableStateOf<String?>(null) }
    var name by remember { mutableStateOf<String?>(null) }
    var relay by remember { mutableStateOf(Repo.relay) }
    var whitelisted by remember { mutableStateOf(BatteryOpt.isIgnoring(ctx)) }
    var saved by remember { mutableStateOf<String?>(null) }
    /** 待确认的新中继地址（换中继要先告诉用户代价） */
    var pendingRelay by remember { mutableStateOf<String?>(null) }
    var relayBusy by remember { mutableStateOf(false) }

    LaunchedEffect(tick) {
        withContext(Dispatchers.IO) {
            myId = Repo.identity()
            name = Repo.selfName()
        }
    }

    Column(
        Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp)
    ) {
        // ---- 身份
        Card(Modifier.fillMaxWidth()) {
            Column(Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                Text(stringResource(R.string.settings_identity), style = MaterialTheme.typography.titleSmall)
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text(
                        name ?: "…",
                        style = MaterialTheme.typography.titleMedium,
                        modifier = Modifier.weight(1f)
                    )
                    TextButton(onClick = {
                        myId?.let {
                            clip.setText(AnnotatedString(it))
                            saved = ctx.getString(R.string.settings_copied_id)
                        }
                    }) { Text(stringResource(R.string.settings_copy_id)) }
                }
                Text(
                    myId ?: "",
                    fontSize = 10.sp,
                    fontFamily = FontFamily.Monospace,
                    color = MaterialTheme.colorScheme.onSurfaceVariant
                )
                Text(
                    stringResource(R.string.settings_id_hint),
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant
                )
                var showPhrase by remember { mutableStateOf(false) }
                TextButton(onClick = { showPhrase = true }) { Text(stringResource(R.string.settings_view_phrase)) }
                if (showPhrase) {
                    PhraseDialog(onClose = { showPhrase = false })
                }
            }
        }

        // ---- 接收服务
        Card(Modifier.fillMaxWidth()) {
            Column(Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                val st = MessagingService.state
                Text(stringResource(R.string.settings_receiving), style = MaterialTheme.typography.titleSmall)
                Text(
                    if (st.running)
                        stringResource(R.string.settings_running, st.uptimeSecs / 60, st.reconnects)
                    else stringResource(R.string.settings_not_running),
                    style = MaterialTheme.typography.bodySmall
                )
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    if (st.running) {
                        OutlinedButton(onClick = onStopService) { Text(stringResource(R.string.action_stop)) }
                    } else {
                        Button(onClick = onStartService) { Text(stringResource(R.string.action_start)) }
                    }
                }
            }
        }

        // ---- 电池白名单
        Card(Modifier.fillMaxWidth()) {
            Column(Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                Text(stringResource(R.string.settings_battery_title), style = MaterialTheme.typography.titleSmall)
                Text(
                    if (whitelisted) stringResource(R.string.settings_battery_ok)
                    else stringResource(R.string.settings_battery_missing),
                    style = MaterialTheme.typography.bodySmall
                )
                if (!whitelisted) {
                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                        Button(onClick = {
                            BatteryOpt.requestIgnore(ctx)
                            whitelisted = BatteryOpt.isIgnoring(ctx)
                        }) { Text(stringResource(R.string.chats_banner_grant)) }
                        OutlinedButton(onClick = { BatteryOpt.openOemSettings(ctx) }) {
                            Text(stringResource(R.string.settings_oem_autostart))
                        }
                    }
                }
            }
        }

        // ---- 中继
        Row(verticalAlignment = Alignment.CenterVertically) {
            OutlinedTextField(
                value = relay, onValueChange = { relay = it },
                label = { Text(stringResource(R.string.settings_relay_label)) }, singleLine = true,
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri),
                modifier = Modifier.weight(1f)
            )
            var showPicker by remember { mutableStateOf(false) }
            TextButton(onClick = { showPicker = true }) { Text(stringResource(R.string.settings_relay_list)) }
            if (showPicker) {
                RelayPickerDialog(
                    current = relay,
                    onPick = { relay = it },
                    onClose = { showPicker = false }
                )
            }
        }
        Text(
            stringResource(R.string.settings_relay_hint),
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant
        )

        // ---- 通话网络
        IceSettings(ctx)

        saved?.let {
            Text(
                it,
                color = MaterialTheme.colorScheme.primary,
                style = MaterialTheme.typography.bodySmall
            )
        }

        Button(
            onClick = {
                val target = relay.trim()
                if (target != Repo.relay) {
                    // 换中继是"搬家"，不是改个字符串：旧中继队列里未送达的消息不会跟过来。
                    // 弹出确认让用户明确知道代价，而不是保存完才发现对方收不到。
                    pendingRelay = target
                } else {
                    Repo.setRelay(ctx, target)
                    saved = ctx.getString(R.string.settings_saved)
                    onChanged()
                }
            },
            modifier = Modifier.fillMaxWidth().height(48.dp)
        ) { Text(stringResource(R.string.settings_save)) }

        // 换中继确认：旧中继上的未送达消息不会跟过来，必须先讲清楚
        pendingRelay?.let { target ->
            AlertDialog(
                onDismissRequest = { if (!relayBusy) pendingRelay = null },
                title = { Text(stringResource(R.string.settings_switch_relay_title)) },
                text = {
                    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                        Text(stringResource(R.string.settings_switch_new_addr, target), style = MaterialTheme.typography.bodySmall)
                        Text(
                            stringResource(R.string.settings_switch_body),
                            style = MaterialTheme.typography.bodySmall
                        )
                        if (relayBusy) Text(stringResource(R.string.settings_switch_verifying), style = MaterialTheme.typography.bodySmall)
                    }
                },
                confirmButton = {
                    TextButton(
                        enabled = !relayBusy,
                        onClick = {
                            relayBusy = true
                            kotlinx.coroutines.MainScope().launch {
                                val r = withContext(Dispatchers.IO) { Repo.setRelay(ctx, target) }
                                relayBusy = false
                                pendingRelay = null
                                if (r.ok) {
                                    saved = ctx.getString(R.string.settings_switch_done)
                                    relay = target
                                    onChanged()
                                } else {
                                    saved = ctx.getString(R.string.settings_switch_failed, errText(ctx, r))
                                }
                            }
                        }
                    ) { Text(stringResource(R.string.settings_switch_action)) }
                },
                dismissButton = {
                    TextButton(enabled = !relayBusy, onClick = { pendingRelay = null }) { Text(stringResource(R.string.action_cancel)) }
                }
            )
        }

        // 版本号从 APK 里读。之前是硬编码字符串，一路从 0.1.0 带到 0.6.0 都没改过，
        // 装到手机上显示的版本号和 APK 实际版本对不上，排查问题时会误导。
        val ver = remember {
            runCatching {
                ctx.packageManager.getPackageInfo(ctx.packageName, 0).versionName
            }.getOrNull().orEmpty()
        }
        // 版本号单独一行、加大加粗：之前一行小灰字（labelSmall 11sp）埋在页底，
        // 用户找不到版本信息，报障时说不清装的是哪版
        Text(
            stringResource(R.string.app_name) + " $ver",
            style = MaterialTheme.typography.titleMedium,
            fontWeight = FontWeight.Medium,
            color = MaterialTheme.colorScheme.onSurface,
            modifier = Modifier.fillMaxWidth(),
            textAlign = TextAlign.Center
        )
        Text(
            stringResource(R.string.settings_foss_note),
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.fillMaxWidth(),
            textAlign = TextAlign.Center
        )
        Spacer(Modifier.height(12.dp))
    }
}

/**
 * 通话网络设置（STUN / TURN）。
 *
 * 不配 TURN 时，通话只在**同一局域网或同运营商**能连通；
 * 跨网段（比如一个在家宽、一个在移动网）几乎必然连不上媒体通道。
 * 这不是 bug，是 P2P 的物理限制，只能靠 TURN 中继。
 */
/**
 * 探测结果的文案。放在 UI 层而不是 IceProbe 里：数据类不该依赖 Context，
 * 这里也正好示范"错误码 + UI 映射"的模式（数据层错误消息国际化的模板）。
 * 返回 (文案, 是否用强调色)——✅/❌ 用主色，⚠️ 用次级色，与旧版观感一致。
 */
@Composable
private fun probeSummary(p: CallManager.IceProbe): Pair<String, Boolean> = when {
    p.errorKind != null -> when (p.errorKind) {
        CallManager.IceErr.NOT_INIT -> stringResource(R.string.ice_err_not_init) to true
        CallManager.IceErr.PC_FAILED -> stringResource(R.string.ice_err_pc_failed) to true
    }
    p.relay > 0 -> stringResource(R.string.ice_ok_turn, p.relay) to true
    p.hasTurn -> stringResource(R.string.ice_fail_turn) to true
    p.srflx > 0 -> stringResource(R.string.ice_only_stun) to false
    p.host > 0 -> stringResource(R.string.ice_only_host) to false
    else -> stringResource(R.string.ice_none) to true
}

@Composable
private fun IceSettings(ctx: android.content.Context) {
    var open by remember { mutableStateOf(false) }
    var cfg by remember { mutableStateOf(Repo.iceConfig(ctx)) }
    // 一键配置用的粘贴框 + 探测状态
    var link by remember { mutableStateOf("") }
    var probing by remember { mutableStateOf(false) }
    var probeMsg by remember { mutableStateOf<String?>(null) }
    var probeResult by remember { mutableStateOf<CallManager.IceProbe?>(null) }

    fun save() = Repo.setIceConfig(
        ctx,
        cfg["stun"] ?: "", cfg["turn"] ?: "",
        cfg["turn_user"] ?: "", cfg["turn_pass"] ?: ""
    )

    Card(Modifier.fillMaxWidth()) {
        Column(Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            Row(
                Modifier.fillMaxWidth().clickable { open = !open },
                verticalAlignment = Alignment.CenterVertically
            ) {
                Text(
                    stringResource(R.string.ice_title),
                    style = MaterialTheme.typography.titleSmall,
                    modifier = Modifier.weight(1f)
                )
                Text(
                    if (open) stringResource(R.string.action_collapse) else stringResource(R.string.action_expand),
                    style = MaterialTheme.typography.labelMedium,
                    color = MaterialTheme.colorScheme.primary
                )
            }
            if (open) {
                Text(
                    stringResource(R.string.ice_hint),
                    style = MaterialTheme.typography.bodySmall
                )
                // 一键配置：把一条链接拆成 地址 + 账号 + 密码，省得对着三个框抄
                OutlinedTextField(
                    value = link,
                    onValueChange = { link = it; probeMsg = null },
                    label = { Text(stringResource(R.string.ice_paste_label)) },
                    singleLine = true,
                    keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri),
                    modifier = Modifier.fillMaxWidth()
                )
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    OutlinedButton(
                        enabled = link.isNotBlank(),
                        onClick = {
                            val t = CallManager.parseTurnLink(link)
                            if (t == null) {
                                probeMsg = ctx.getString(R.string.ice_bad_format)
                            } else {
                                cfg = cfg + ("turn" to t.url) +
                                    ("turn_user" to t.user.ifBlank { cfg["turn_user"] ?: "" }) +
                                    ("turn_pass" to t.pass.ifBlank { cfg["turn_pass"] ?: "" })
                                save()
                                link = ""
                                probeMsg = ctx.getString(R.string.ice_filled, t.url)
                            }
                        }
                    ) { Text(stringResource(R.string.ice_parse_fill)) }
                    OutlinedButton(
                        enabled = !probing,
                        onClick = {
                            probing = true
                            probeResult = null
                            probeMsg = ctx.getString(R.string.ice_probing)
                            kotlinx.coroutines.MainScope().launch {
                                val r = withContext(Dispatchers.IO) {
                                    CallManager.probeIce(
                                        cfg["stun"] ?: "", cfg["turn"] ?: "",
                                        cfg["turn_user"] ?: "", cfg["turn_pass"] ?: ""
                                    )
                                }
                                probeMsg = null
                                probeResult = r
                                probing = false
                            }
                        }
                    ) { Text(if (probing) stringResource(R.string.ice_testing) else stringResource(R.string.ice_test)) }
                }
                probeMsg?.let {
                    Text(
                        it,
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant
                    )
                }
                probeResult?.let { p ->
                    val (text, accent) = probeSummary(p)
                    Text(
                        text + if (p.timedOut) " " + stringResource(R.string.ice_probe_timeout) else "",
                        style = MaterialTheme.typography.bodySmall,
                        color = if (accent) MaterialTheme.colorScheme.primary
                        else MaterialTheme.colorScheme.onSurfaceVariant
                    )
                }
                OutlinedTextField(
                    value = cfg["stun"] ?: "",
                    onValueChange = { cfg = cfg + ("stun" to it); save() },
                    label = { Text(stringResource(R.string.ice_stun_label)) },
                    singleLine = true, modifier = Modifier.fillMaxWidth()
                )
                OutlinedTextField(
                    value = cfg["turn"] ?: "",
                    onValueChange = { cfg = cfg + ("turn" to it); save() },
                    label = { Text(stringResource(R.string.ice_turn_label)) },
                    singleLine = true,
                    keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri),
                    modifier = Modifier.fillMaxWidth()
                )
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    OutlinedTextField(
                        value = cfg["turn_user"] ?: "",
                        onValueChange = { cfg = cfg + ("turn_user" to it); save() },
                        label = { Text(stringResource(R.string.ice_username)) }, singleLine = true, modifier = Modifier.weight(1f)
                    )
                    OutlinedTextField(
                        value = cfg["turn_pass"] ?: "",
                        onValueChange = { cfg = cfg + ("turn_pass" to it); save() },
                        label = { Text(stringResource(R.string.ice_password)) }, singleLine = true, modifier = Modifier.weight(1f)
                    )
                }
            }
        }
    }
}

// ---------------------------------------------------------------- 恢复短语对话框

/**
 * 设置页查看恢复短语。两段式：先警告再显示。
 *
 * 为什么不直接显示：这串词等于身份本体，弹窗出现的那一刻屏幕上就趴着全部秘密。
 * 先让用户确认"周围没人、没摄像头"，是零成本但真实有效的防窥手段。
 */
@Composable
private fun PhraseDialog(onClose: () -> Unit) {
    var revealed by remember { mutableStateOf(false) }
    var phrase by remember { mutableStateOf<String?>(null) }
    var err by remember { mutableStateOf<String?>(null) }
    val clip = LocalClipboardManager.current
    // 读取失败那句在协程里（不是组合期），stringResource 用不了，得拿 Context
    val ctx = androidx.compose.ui.platform.LocalContext.current

    AlertDialog(
        onDismissRequest = onClose,
        title = { Text(stringResource(R.string.phrase_dlg_title)) },
        text = {
            Column {
                when {
                    !revealed -> {
                        Text(stringResource(R.string.phrase_dlg_warn_identity))
                        Text(
                            stringResource(R.string.phrase_dlg_warn_surveillance),
                            color = MaterialTheme.colorScheme.error,
                            style = MaterialTheme.typography.bodySmall
                        )
                    }
                    phrase != null -> {
                        val words = phrase!!.split(" ")
                        Card(Modifier.fillMaxWidth()) {
                            Column(
                                Modifier.padding(12.dp),
                                verticalArrangement = Arrangement.spacedBy(6.dp)
                            ) {
                                words.chunked(3).forEachIndexed { rowIdx, rowWords ->
                                    Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceEvenly) {
                                        rowWords.forEachIndexed { colIdx, w ->
                                            Text(
                                                "${rowIdx * 3 + colIdx + 1}. $w",
                                                fontFamily = FontFamily.Monospace,
                                                fontSize = 14.sp
                                            )
                                        }
                                    }
                                }
                            }
                        }
                        Spacer(Modifier.height(8.dp))
                        TextButton(onClick = {
                            clip.setText(AnnotatedString(phrase!!))
                        }) { Text(stringResource(R.string.phrase_dlg_copy)) }
                    }
                    err != null -> Text(err!!, color = MaterialTheme.colorScheme.error)
                    else -> Text(stringResource(R.string.phrase_dlg_loading))
                }
            }
        },
        confirmButton = {
            TextButton(onClick = {
                if (!revealed) {
                    kotlinx.coroutines.MainScope().launch {
                        val r = withContext(Dispatchers.IO) { Repo.backupPhrase() }
                        if (r.ok && r.data != null) {
                            phrase = r.data
                            revealed = true
                        } else err = errText(ctx, r)
                    }
                } else onClose()
            }) { Text(if (!revealed) stringResource(R.string.phrase_dlg_show) else stringResource(R.string.action_done)) }
        },
        dismissButton = {
            if (!revealed) TextButton(onClick = onClose) { Text(stringResource(R.string.action_cancel)) }
        }
    )
}

// ---------------------------------------------------------------- 中继选择对话框

/** 设置页的紧凑版中继选择：点一条就填进中继地址框，也可手输 */
@Composable
private fun RelayPickerDialog(
    current: String,
    onPick: (String) -> Unit,
    onClose: () -> Unit,
) {
    val ctx = androidx.compose.ui.platform.LocalContext.current
    var list by remember { mutableStateOf<List<RelayEntry>>(emptyList()) }
    var latency by remember { mutableStateOf<Map<String, Long>>(emptyMap()) }
    var custom by remember { mutableStateOf("") }
    // 与首次引导页同一套逻辑；这里单独放一份因为弹窗的布局不同
    var discovering by remember { mutableStateOf(false) }
    var dhtFound by remember { mutableStateOf<Int?>(null) }
    val scope = rememberCoroutineScope()

    LaunchedEffect(Unit) {
        val (l, _) = withContext(Dispatchers.IO) { RelayList.load(ctx) }
        list = l
        if (l.isNotEmpty()) latency = RelayList.probeAll(l.map { it.url })
    }

    AlertDialog(
        onDismissRequest = onClose,
        title = { Text(stringResource(R.string.relay_pick_title)) },
        text = {
            Column {
                Text(
                    stringResource(R.string.relay_pick_hint),
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant
                )
                Spacer(Modifier.height(8.dp))

                // 只放在首次引导页是不够的：已经装过 App 的人只会进这个弹窗，
                // 永远看不到那个按钮。两处都要有。
                OutlinedButton(
                    enabled = !discovering,
                    onClick = {
                        discovering = true
                        scope.launch {
                            val r = withContext(Dispatchers.IO) { Repo.discoverRelays() }
                            val got = r.data.orEmpty()
                            if (got.isNotEmpty()) {
                                val known = list.map { it.url }.toSet()
                                val added = got.filter { it !in known }
                                    .map { RelayEntry(it, "", "") }
                                list = list + added
                                // 只测新来的，别覆盖已有结果
                                latency = latency + RelayList.probeAll(added.map { it.url })
                            }
                            dhtFound = got.size
                            discovering = false
                        }
                    },
                    modifier = Modifier.fillMaxWidth()
                ) {
                    Text(
                        if (discovering) stringResource(R.string.relay_dht_discovering)
                        else stringResource(R.string.relay_dht_discover)
                    )
                }
                when {
                    discovering -> Text(
                        stringResource(R.string.relay_dht_hint),
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant
                    )
                    dhtFound == 0 -> Text(
                        stringResource(R.string.relay_dht_none),
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.error
                    )
                    dhtFound != null && dhtFound!! > 0 -> Text(
                        stringResource(R.string.relay_dht_found, dhtFound!!),
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.primary
                    )
                }
                Spacer(Modifier.height(8.dp))

                if (list.isEmpty()) {
                    Text(stringResource(R.string.relay_pick_empty), style = MaterialTheme.typography.bodySmall)
                } else {
                    Column(
                        Modifier
                            .fillMaxWidth()
                            .height(280.dp)
                            .verticalScroll(rememberScrollState()),
                        verticalArrangement = Arrangement.spacedBy(6.dp)
                    ) {
                        list.sortedBy { e -> latency[e.url]?.takeIf { it >= 0 } ?: Long.MAX_VALUE }
                            .forEach { e ->
                                val ms = latency[e.url]
                                Card(
                                    Modifier.fillMaxWidth().clickable { onPick(e.url); onClose() }
                                ) {
                                    Row(
                                        Modifier.padding(10.dp),
                                        verticalAlignment = Alignment.CenterVertically
                                    ) {
                                        Column(Modifier.weight(1f)) {
                                            Text(
                                                e.url,
                                                fontSize = 11.sp,
                                                fontFamily = FontFamily.Monospace
                                            )
                                            Text(
                                                e.region.ifBlank { stringResource(R.string.relay_region_unknown) },
                                                style = MaterialTheme.typography.labelSmall,
                                                color = MaterialTheme.colorScheme.onSurfaceVariant
                                            )
                                        }
                                        Text(
                                            when {
                                                ms == null -> stringResource(R.string.relay_probing)
                                                ms < 0 -> stringResource(R.string.relay_unreachable)
                                                else -> "${ms}ms"
                                            },
                                            style = MaterialTheme.typography.labelSmall
                                        )
                                    }
                                }
                            }
                    }
                }
                Spacer(Modifier.height(8.dp))
                OutlinedTextField(
                    value = custom, onValueChange = { custom = it },
                    label = { Text(stringResource(R.string.relay_pick_custom)) },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth()
                )
                TextButton(
                    enabled = custom.isNotBlank(),
                    onClick = { onPick(custom.trim()); onClose() }
                ) { Text(stringResource(R.string.relay_pick_use)) }
            }
        },
        confirmButton = {},
        dismissButton = { TextButton(onClick = onClose) { Text(stringResource(R.string.action_close)) } }
    )
}

// ---------------------------------------------------------------- 二维码

/** 邀请码 → QR 位图。邀请码 350+ 字符，纠错用 L 级别压低版本号 */
private fun qrBitmap(content: String, size: Int): android.graphics.Bitmap {
    val hints = mapOf(
        com.google.zxing.EncodeHintType.MARGIN to 1,
        com.google.zxing.EncodeHintType.ERROR_CORRECTION to
            com.google.zxing.qrcode.decoder.ErrorCorrectionLevel.L,
    )
    val matrix = com.google.zxing.qrcode.QRCodeWriter()
        .encode(content, com.google.zxing.BarcodeFormat.QR_CODE, size, size, hints)
    val pixels = IntArray(size * size)
    for (y in 0 until size) {
        for (x in 0 until size) {
            pixels[y * size + x] =
                if (matrix[x, y]) android.graphics.Color.BLACK else android.graphics.Color.WHITE
        }
    }
    return android.graphics.Bitmap.createBitmap(pixels, size, size, android.graphics.Bitmap.Config.RGB_565)
}

// ---------------------------------------------------------------- 聊天页

/** 聊天页一页加载多少条。60 条 ≈ 两屏多，滚到底再要一页，用户察觉不到接缝 */
private const val MSG_PAGE = 60

/**
 * 聊天页顶栏：‹ + 居中标题 + ⋯。
 *
 * 为什么把「语音」「视频」收进 ⋯：两个描边按钮常年占着标题右侧，标题只能挤在左边，
 * 而通话是低频操作。收起来之后标题才真的居中。
 * 安全码仍然绑在标题上（点名字直接进）—— 那是用户唯一能"确认对面是谁"的入口，
 * 藏进 ⋯ 里就没人会主动点了。
 */
@Composable
private fun ChatTopBar(
    peer: String,
    verified: Boolean,
    canCall: Boolean,
    panicCode: String?,
    onBack: () -> Unit,
    onSafety: () -> Unit,
    onCall: (video: Boolean) -> Unit,
    onPanic: () -> Unit,
) {
    val skin = LocalSkin.current
    var more by remember { mutableStateOf(false) }
    // peer 是会话键（dh），显示名单独解析：这样改备注名/对方改名都不影响会话本身
    val label = displayNameOf(peer)
    Row(
        Modifier.fillMaxWidth().height(SkinDims.barHeight).background(skin.bar),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(
            Modifier.size(SkinDims.barHeight).clickable(onClick = onBack),
            contentAlignment = Alignment.Center,
        ) {
            // 细箭头而不是文字"返回"：文字按钮会把标题顶到右边，居中就无从谈起
            Text("‹", fontSize = 30.sp, color = skin.icon)
        }
        Row(
            Modifier.weight(1f).clickable(onClick = onSafety),
            horizontalArrangement = Arrangement.Center,
            verticalAlignment = Alignment.CenterVertically,
        ) {
            // 顶栏小头像：和会话列表同色（名字哈希），聊天页里也能对上号
            Avatar(label, 28.dp)
            Spacer(Modifier.width(6.dp))
            Text(
                label,
                fontSize = 16.sp, fontWeight = FontWeight.Medium, color = skin.icon,
                maxLines = 1, overflow = TextOverflow.Ellipsis,
                modifier = Modifier.weight(1f, fill = false),
            )
            if (verified) {
                Spacer(Modifier.width(4.dp))
                Text("✓", fontSize = 12.sp, color = skin.accent)
            }
        }
        Box(contentAlignment = Alignment.Center) {
            Box(
                Modifier.size(SkinDims.barHeight).clickable(enabled = canCall) { more = true },
                contentAlignment = Alignment.Center,
            ) {
                Text(
                    "···", fontSize = 20.sp,
                    color = if (canCall) skin.icon else skin.placeholder,
                )
            }
            DropdownMenu(expanded = more, onDismissRequest = { more = false }) {
                DropdownMenuItem(
                    text = { Text(stringResource(R.string.menu_call_voice)) },
                    enabled = canCall,
                    onClick = { more = false; onCall(false) },
                )
                DropdownMenuItem(
                    text = { Text(stringResource(R.string.menu_call_video)) },
                    enabled = canCall,
                    onClick = { more = false; onCall(true) },
                )
                DropdownMenuItem(
                    text = {
                        Text(
                            if (verified) stringResource(R.string.menu_safety_verified)
                            else stringResource(R.string.menu_safety)
                        )
                    },
                    onClick = { more = false; onSafety() },
                )
                if (panicCode != null) {
                    DropdownMenuItem(
                        text = { Text(stringResource(R.string.menu_panic), color = MaterialTheme.colorScheme.error) },
                        onClick = { more = false; onPanic() },
                    )
                }
            }
        }
    }
    HorizontalDivider(color = skin.divider, thickness = 0.5.dp)
}


@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun ChatScreen(
    peer: String,
    onBack: () -> Unit,
    onStartCall: (String, Boolean) -> Unit,
    onPeerRenamed: (String) -> Unit,
) {
    // 只把视口附近的消息交给 LazyColumn：几千条历史全塞进去，每次刷新都要
    // 整个列表做 diff，滑动明显掉帧；而且 messagesOf() 会拷贝整个会话。
    // windowSize 是当前加载的条数，往上滚到底时再加一页。
    var msgs by remember(peer) { mutableStateOf(listOf<Msg>()) }
    var windowSize by remember(peer) { mutableStateOf(MSG_PAGE) }
    var totalMsgs by remember(peer) { mutableStateOf(0) }
    /** 当前 msgs[0] 在整个会话里的下标，用于判断"还有没有更早的" */
    var shownFrom by remember(peer) { mutableStateOf(0) }
    /** 加载更早消息后要滚到的位置（新增条数），-1 = 不需要补偿 */
    var anchor by remember(peer) { mutableStateOf(-1) }
    /** 用户是否停在底部：只有停在底部时新消息才自动滚下去，否则阅读历史会被顶走 */
    var stickBottom by remember(peer) { mutableStateOf(true) }
    /** 本地操作（自己发消息）也要触发一次重新加载 */
    var localBump by remember(peer) { mutableStateOf(0) }
    var input by remember { mutableStateOf("") }
    var err by remember { mutableStateOf<String?>(null) }
    var busy by remember { mutableStateOf(false) }
    // 对方是否已经是联系人：不是的话只能看不能回，需要在输入栏位置给出"加为联系人"
    var isContact by remember(peer) { mutableStateOf(true) }
    // 有没有对方的回信线索（旧版本客户端发来的消息不带，这时连加都加不了）
    var hasCard by remember(peer) { mutableStateOf(false) }
    // 文件发送进度：null = 没在发
    var fileSending by remember { mutableStateOf<TxFile?>(null) }
    // 安全码对话框 + "已核对"角标
    var showSafety by remember { mutableStateOf(false) }
    // 紧急清除：暗号（双方派生一致，null=拿不到对方公钥）与弹窗开关
    val panicCode = remember(peer) { Repo.panicCodeOf(peer) }
    var showPanic by remember { mutableStateOf(false) }
    var verifiedBadge by remember(peer) { mutableStateOf(false) }
    // 文件气泡点开的菜单。状态放在页面这一层：LazyColumn 的 item 会被回收，
    // 状态塞进 item 里滚动一下就没了。
    var fileMenu by remember(peer) { mutableStateOf<FileBubble?>(null) }
    // 菜单对应的原消息：撤回要拿它的 rid 和时间戳
    var fileMenuMsg by remember(peer) { mutableStateOf<Msg?>(null) }
    // 「另存为」等系统选择器返回时，得记得是哪个文件
    var pendingSave by remember { mutableStateOf<FileBubble?>(null) }
    // 📎 点开后要选的是图片、视频还是文件
    var attachMenu by remember { mutableStateOf(false) }
    // 阅后即焚：当前档位（0=关）。按条生效——选中后下一条消息（文字/图片/视频/
    // 文件/语音都算）带焚毁标记，发出去就自动复位，避免"忘了关"整场都在烧
    var burnOpt by remember { mutableStateOf(0L) }
    // 焚毁倒计时每秒走一格；只在会话里真有焚毁消息时才跑这个循环
    var burnTick by remember(peer) { mutableStateOf(0) }
    // 正在全屏查看的图片/视频。放在页面这一层：气泡会被 LazyColumn 回收，
    // 对话框状态塞在 item 里滚一下就没了
    var mediaView by remember(peer) { mutableStateOf<MediaBubble?>(null) }
    // ---- 多选：长按菜单进入，勾选后批量删除/转发/复制
    var selecting by remember(peer) { mutableStateOf(false) }
    // 剪贴板要在 composable 作用域取一次：批量复制发生在 onClick 里，那里不能调 @Composable
    val clipForCopy = LocalClipboardManager.current
    var selected by remember(peer) { mutableStateOf(setOf<Long>()) }
    var forwardOpen by remember(peer) { mutableStateOf(false) }
    /** 待确认删除的消息 id 集合 */
    var delMsgs by remember(peer) { mutableStateOf<Set<Long>?>(null) }
    // ---- 语音：按住说话
    var recording by remember { mutableStateOf(false) }
    var recMs by remember { mutableStateOf(0L) }
    /** 最近若干个电平值，画实时波形。麦克风真的在收声音时它会跳 */
    var recLevels by remember { mutableStateOf(listOf<Int>()) }
    /** 手指已经上滑到取消区 */
    var recCancelling by remember { mutableStateOf(false) }
    val ctx = androidx.compose.ui.platform.LocalContext.current
    val skin = LocalSkin.current
    val listState = androidx.compose.foundation.lazy.rememberLazyListState()

    // 录音权限：只在真的按麦克风时才要。不给也不影响打字和发文件
    val recPerm = androidx.activity.compose.rememberLauncherForActivityResult(
        ActivityResultContracts.RequestPermission()
    ) { ok ->
        android.widget.Toast.makeText(
            ctx,
            if (ok) ctx.getString(R.string.mic_ok) else ctx.getString(R.string.mic_denied),
            android.widget.Toast.LENGTH_SHORT
        ).show()
    }

    // 「另存为」：用户自己挑位置（SD 卡、网盘都行），不需要任何权限
    val saveAs = androidx.activity.compose.rememberLauncherForActivityResult(
        androidx.activity.result.contract.ActivityResultContracts.CreateDocument("*/*")
    ) { uri ->
        val fb = pendingSave
        pendingSave = null
        if (uri == null || fb == null) return@rememberLauncherForActivityResult
        val src = fb.file
        if (src == null) {
            android.widget.Toast.makeText(ctx, ctx.getString(R.string.file_gone), android.widget.Toast.LENGTH_SHORT).show()
            return@rememberLauncherForActivityResult
        }
        kotlinx.coroutines.MainScope().launch {
            val ok = withContext(Dispatchers.IO) { FileSaver.copyTo(ctx, src, uri) }
            android.widget.Toast.makeText(
                ctx, if (ok) ctx.getString(R.string.settings_saved) else ctx.getString(R.string.file_save_failed), android.widget.Toast.LENGTH_SHORT
            ).show()
        }
    }

    /**
     * 存到「下载/AnonChat」。
     *
     * Android 10+ 一键落盘不用选目录；Android 8/9 写公共目录要申请全盘存储权限，
     * 为一次保存去要那个权限不划算，直接转「另存为」让用户自己在选择器里挑位置。
     */
    fun saveFile(fb: FileBubble) {
        val src = fb.file
        if (src == null) {
            android.widget.Toast.makeText(ctx, ctx.getString(R.string.file_gone), android.widget.Toast.LENGTH_SHORT).show()
            return
        }
        if (!FileSaver.canAutoSave()) {
            pendingSave = fb
            saveAs.launch(fb.name)
            return
        }
        kotlinx.coroutines.MainScope().launch {
            val where = withContext(Dispatchers.IO) { FileSaver.saveToDownloads(ctx, fb.name, src) }
            android.widget.Toast.makeText(
                ctx,
                where?.let { ctx.getString(R.string.file_saved_to, it) } ?: ctx.getString(R.string.file_save_check_storage),
                android.widget.Toast.LENGTH_LONG
            ).show()
        }
    }

    // 文件选择器：只查名字和大小，内容边读边发（20MB 整块读进内存会顶爆低端机）
    val pickFile = androidx.activity.compose.rememberLauncherForActivityResult(
        androidx.activity.result.contract.ActivityResultContracts.OpenDocument()
    ) { uri ->
        if (uri == null) return@rememberLauncherForActivityResult
        kotlinx.coroutines.MainScope().launch {
            val meta = withContext(Dispatchers.IO) {
                runCatching {
                    var name = ctx.getString(R.string.file_default_name)
                    var size = -1L
                    ctx.contentResolver.query(uri, null, null, null, null)?.use { c ->
                        if (c.moveToFirst()) {
                            val ni = c.getColumnIndex(android.provider.OpenableColumns.DISPLAY_NAME)
                            val si = c.getColumnIndex(android.provider.OpenableColumns.SIZE)
                            if (ni >= 0 && !c.isNull(ni)) name = c.getString(ni)
                            if (si >= 0 && !c.isNull(si)) size = c.getLong(si)
                        }
                    }
                    name to size
                }.getOrNull()
            }
            if (meta == null) { err = ctx.getString(R.string.file_unreadable); return@launch }
            val (name, size) = meta
            if (size <= 0) { err = ctx.getString(R.string.file_no_size); return@launch }
            if (size > Repo.FILE_MAX_BYTES) {
                err = ctx.getString(R.string.file_too_big, Repo.FILE_MAX_BYTES / 1024 / 1024); return@launch
            }
            err = null
            fileSending = TxFile(name, 0, size)
            val res = withContext(Dispatchers.IO) {
                Repo.sendFile(
                    peer, name, size,
                    open = { ctx.contentResolver.openInputStream(uri)!! },
                    burn = burnOpt,
                ) { sent, total -> fileSending = TxFile(name, sent, total) }
            }
            fileSending = null
            if (res.ok) {
                stickBottom = true
                localBump++      // 让列表重新加载并滚到底
                burnOpt = 0
            } else err = res.error
        }
    }

    /**
     * 图片的统一出口：不管是相册选的还是刚拍的，后面完全一样——
     * 本地压缩 → 分块发出去 → 删临时文件。
     *
     * [temp] 是拍照落在缓存里的原图，发完（或失败）要删掉；相册选的图为 null。
     */
    fun submitImage(uri: android.net.Uri, temp: java.io.File? = null) {
        kotlinx.coroutines.MainScope().launch {
            err = null
            // 压缩要几百毫秒到一两秒，先给个"正在处理"的进度，否则点完像是没反应
            fileSending = TxFile(ctx.getString(R.string.media_image), 0, 0, media = "image")
            val prep = withContext(Dispatchers.IO) {
                ImageCompress.prepare(ctx, uri, java.io.File(ctx.cacheDir, "outbox"))
            }
            val out = prep.data
            if (!prep.ok || out == null) {
                fileSending = null
                temp?.delete()
                err = errText(ctx, prep)
                return@launch
            }
            fileSending = TxFile(out.file.name, 0, out.size, media = "image")
            val r = withContext(Dispatchers.IO) {
                Repo.sendFile(
                    peer, out.file.name, out.size,
                    open = { out.file.inputStream() },
                    kind = "image",
                    width = out.w, height = out.h,
                    burn = burnOpt,
                ) { sent, total -> fileSending = TxFile(out.file.name, sent, total, media = "image") }
            }
            fileSending = null
            // 发完删临时文件：sent/ 里已经留了副本，这份留着只是占缓存
            out.file.delete()
            temp?.delete()
            if (r.ok) {
                stickBottom = true
                localBump++
                burnOpt = 0   // 阅后即焚按条生效：发一条就复位
            } else err = errText(ctx, r)
        }
    }

    /**
     * 从相册发一张图。
     *
     * 用 PickVisualMedia（系统照片选择器）而不是通用的"选任意文件"入口：
     * 它只列相册里的图，用户不用在几百个目录里翻；而且**不需要任何存储权限**，
     * 系统只把用户选中的那几张授权给我们。
     */
    val pickImage = rememberLauncherForActivityResult(
        ActivityResultContracts.PickVisualMedia()
    ) { uri ->
        if (uri != null) submitImage(uri)
    }

    // 刚拍的照片落在 cache/shots，压完就删——不进系统相册，也就不会被别的 App 扫到。
    // 用 rememberSaveable 存路径：相机是独立进程，内存紧时我们可能被系统回收，
    // 回来时这个值必须还在，否则表现就是"拍完没反应"。
    var shotPath by androidx.compose.runtime.saveable.rememberSaveable {
        mutableStateOf<String?>(null)
    }

    val takePicture = rememberLauncherForActivityResult(
        ActivityResultContracts.TakePicture()
    ) { ok ->
        val f = shotPath?.let { java.io.File(it) }
        shotPath = null
        if (f == null) return@rememberLauncherForActivityResult
        // ok=false 是用户自己从相机退出来了，不是错误，别弹提示
        if (!ok) { f.delete(); return@rememberLauncherForActivityResult }
        // 有些机型返回 ok=true 但一个字节都没写（相机被杀、存储满），先挡一层，
        // 否则要等到压缩阶段才报"解不开这张图"，用户根本不知道发生了什么
        if (!f.exists() || f.length() == 0L) {
            f.delete()
            err = ctx.getString(R.string.camera_no_photo)
            return@rememberLauncherForActivityResult
        }
        val uri = runCatching {
            androidx.core.content.FileProvider.getUriForFile(ctx, "${ctx.packageName}.files", f)
        }.getOrNull()
        if (uri == null) { f.delete(); err = ctx.getString(R.string.camera_unreadable); return@rememberLauncherForActivityResult }
        submitImage(uri, f)
    }

    /** 拉起系统相机。写文件走 FileProvider：Android 7 起直接给 file:// 会抛 FileUriExposedException */
    fun shootPhoto() {
        val dir = java.io.File(ctx.cacheDir, "shots").apply { mkdirs() }
        // 名字用相机风格：对方存下来是 IMG_20261023_102130.jpg，不是 shot-1758000000.jpg
        val stamp = java.text.SimpleDateFormat("yyyyMMdd_HHmmss", java.util.Locale.US)
            .format(java.util.Date())
        val f = java.io.File(dir, "IMG_$stamp.jpg")
        shotPath = f.absolutePath
        val uri = runCatching {
            androidx.core.content.FileProvider.getUriForFile(ctx, "${ctx.packageName}.files", f)
        }.getOrNull()
        if (uri == null) { err = ctx.getString(R.string.camera_bad_provider); return }
        runCatching { takePicture.launch(uri) }.onFailure {
            f.delete()
            shotPath = null
            err = ctx.getString(R.string.camera_no_app)
        }
    }

    // CAMERA 权限在清单里已经声明（通话要用），声明了就必须在启动拍照前拿到，
    // 否则相机应用会直接拒绝这个请求
    val askCamera = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestPermission()
    ) { granted ->
        if (granted) shootPhoto() else err = ctx.getString(R.string.camera_perm_missing)
    }


    /**
     * 发一段视频：同样走系统选择器，但不做转码（手机上自己写编码器成本太高）。
     * 因此容量校验放在最前面——超了就直说超了多少、怎么办，而不是传到一半失败。
     */
    val pickVideo = rememberLauncherForActivityResult(
        ActivityResultContracts.PickVisualMedia()
    ) { uri ->
        if (uri == null) return@rememberLauncherForActivityResult
        kotlinx.coroutines.MainScope().launch {
            err = null
            val probe = withContext(Dispatchers.IO) { VideoProbe.probe(ctx, uri) }
            val meta = probe.data
            if (!probe.ok || meta == null) {
                err = errText(ctx, probe)
                return@launch
            }
            if (meta.size > Repo.FILE_MAX_BYTES) {
                val (label, limitMb) = Media.tooBigInfo(meta.size, Repo.FILE_MAX_BYTES)
                err = ctx.getString(R.string.media_too_big, label, limitMb)
                return@launch
            }
            fileSending = TxFile(meta.name, 0, meta.size, media = "video")
            val r = withContext(Dispatchers.IO) {
                Repo.sendFile(
                    peer, meta.name, meta.size,
                    open = { ctx.contentResolver.openInputStream(uri)!! },
                    kind = "video",
                    durMs = meta.durMs,
                    width = meta.w, height = meta.h,
                    burn = burnOpt,
                ) { sent, total -> fileSending = TxFile(meta.name, sent, total, media = "video") }
            }
            fileSending = null
            if (r.ok) {
                stickBottom = true
                localBump++
                burnOpt = 0
            } else err = errText(ctx, r)
        }
    }

    /**
     * 结束一次录音。[send] = false 表示手指上滑反悔了。
     *
     * 顺序不能反：先取时长再 stop —— stop() 之后 elapsedMs() 就被清零了；
     * 而"太短"的判断必须在 stop 之前做，因为 MediaRecorder 在极短录制下
     * stop() 会抛异常并写坏文件，我们不能等它炸。
     */
    fun finishRecording(send: Boolean) {
        if (!recording) return
        val ms = VoiceRecorder.elapsedMs()
        recording = false
        recCancelling = false
        if (!send) {
            VoiceRecorder.cancel()
            return
        }
        if (Voice.tooShort(ms)) {
            VoiceRecorder.cancel()
            err = ctx.getString(R.string.voice_too_short)
            return
        }
        val f = VoiceRecorder.stop()
        if (f == null) {
            err = ctx.getString(R.string.voice_record_failed)
            return
        }
        val size = f.length()
        if (size <= 0L) {
            f.delete()
            err = ctx.getString(R.string.voice_empty)
            return
        }
        val name = Voice.fileName()
        err = null
        fileSending = TxFile(name, 0, size, voice = true)
        kotlinx.coroutines.MainScope().launch {
            val r = withContext(Dispatchers.IO) {
                Repo.sendFile(
                    peer, name, size,
                    open = { f.inputStream() },
                    kind = "voice",
                    durMs = ms,
                    burn = burnOpt,
                ) { sent, total -> fileSending = TxFile(name, sent, total, voice = true) }
            }
            fileSending = null
            // 发完就删临时录音：sent/ 里已经留了一份副本，这份留着只会占地方
            f.delete()
            if (r.ok) {
                stickBottom = true
                localBump++
                burnOpt = 0
            } else err = errText(ctx, r)
        }
    }

    // 录音计时 + 电平采样，画实时波形。到 60 秒自动收尾发出去（微信同款上限）
    LaunchedEffect(recording) {
        if (!recording) return@LaunchedEffect
        while (recording) {
            recMs = VoiceRecorder.elapsedMs()
            recLevels = (recLevels + VoiceRecorder.amplitude()).takeLast(26)
            if (recMs >= Voice.MAX_MS) {
                finishRecording(true)
                break
            }
            delay(100)
        }
    }

    // 系统返回键（手势/导航栏）也要回到聊天列表：不拦截的话 back 会直接把
    // Activity 退掉，用户从聊天页按返回就整个退出 App（实测踩过）。
    androidx.activity.compose.BackHandler { onBack() }

    // 告诉服务"我正开着这个会话"：同会话新消息不再弹通知（见 MessagingService.notifyMessage），
    // 同时清掉这个会话的未读数
    androidx.compose.runtime.DisposableEffect(peer) {
        MainActivity.openPeer = peer
        Repo.clearUnread(peer)
        onDispose {
            MainActivity.openPeer = null
            // 离开这一页时必须把麦克风和播放器一起掐掉：
            // 麦克风被一个已经看不见的界面占着，是用户最不能接受的 bug
            VoiceRecorder.abort()
            VoicePlayer.stop()
        }
    }

    /** 重发一条失败的消息（点失败气泡上的红色感叹号或菜单里的"重发"） */
    fun resendFailed(m: Msg) {
        if (busy) return
        busy = true; err = null
        kotlinx.coroutines.MainScope().launch {
            val r = withContext(Dispatchers.IO) { Repo.resend(peer, m) }
            if (r.ok) localBump++ else err = errText(ctx, r)
            busy = false
        }
    }

    /** 紧急清除验证通过后执行：清双方数据，然后杀进程（App 回到初始状态） */
    fun executePanicNow() {
        busy = true
        kotlinx.coroutines.MainScope().launch {
            withContext(Dispatchers.IO) { Repo.executePanic(peer, notifyPeer = true) }
            Runtime.getRuntime().exit(0)
        }
    }

    /** 撤回一条消息（文字气泡长按菜单 / 文件类菜单里的"撤回"） */
    fun recallMsg(m: Msg) {
        if (busy) return
        busy = true; err = null
        kotlinx.coroutines.MainScope().launch {
            val r = withContext(Dispatchers.IO) { Repo.recall(peer, m) }
            if (r.ok) localBump++ else err = errText(ctx, r)
            busy = false
        }
    }

    // 实时刷新：新消息到达（inboxTick 变化）、进入页面（peer 变化）、
    // 往上翻（windowSize 变化）、自己发消息（localBump）都重载
    LaunchedEffect(peer, MessagingService.inboxTick, windowSize, localBump) {
        val (t, from) = withContext(Dispatchers.IO) {
            val t = Repo.messageCount(peer)
            t to (t - windowSize).coerceAtLeast(0)
        }
        val list = withContext(Dispatchers.IO) { Repo.messagesOf(peer, from, t) }
        isContact = withContext(Dispatchers.IO) { Repo.contacts().any { it.dh == peer } }
        hasCard = withContext(Dispatchers.IO) { Repo.cardOf(peer) != null }
        // 窗口起点往左移了（加载更早）：记下新增条数，等列表刷新后把视觉位置补回去
        if (from < shownFrom) anchor = shownFrom - from
        totalMsgs = t
        shownFrom = from
        msgs = list
    }
    // 滚动定位：优先"保持阅读位置"，其次是"新消息滚到底"
    LaunchedEffect(msgs.size, anchor) {
        if (msgs.isEmpty()) return@LaunchedEffect
        val a = anchor
        if (a >= 0) {
            listState.scrollToItem(a)
            anchor = -1
        } else if (stickBottom) {
            listState.scrollToItem(msgs.size - 1)
        }
    }
    // 跟踪是否停在底部 + 往上翻到头时自动加载上一页
    LaunchedEffect(peer) {
        var scrolledAway = false
        snapshotFlow { listState.firstVisibleItemIndex to listState.firstVisibleItemScrollOffset }
            .collect { (idx, off) ->
                if (idx > 0) scrolledAway = true
                // 用可见项判断是否到底：最后一条可见就算在底部
                val last = listState.layoutInfo.visibleItemsInfo.lastOrNull()
                if (last != null) {
                    stickBottom = last.index >= msgs.size - 2
                }
                // 只有用户真的往上翻过才自动加载，否则初次进入就一路把历史全load了
                if (idx == 0 && off == 0 && scrolledAway && shownFrom > 0) {
                    scrolledAway = false
                    windowSize += MSG_PAGE
                }
            }
    }

    // 阅后即焚：进窗口就算"首次展示"（幂等，只记一次）；会话里有焚毁消息时
    // 每秒走一格倒计时。
    // 到期清理的 UI 同步不能依赖 sweepExpired 的返回值：服务层的全局清扫
    // （MessagingService 每轮收消息后）可能先把消息删掉，聊天页再扫就是 0，
    // 早先 `if (>0) localBump++` 于是永远不触发——存储里没了、界面上还挂着，
    // 重进会话才消失（真机实测）。改成直接对比存储快照：焚毁消息从存储里
    // 消失（无论被谁删的）就重载列表。
    LaunchedEffect(peer, msgs) {
        msgs.forEach { Repo.markSeen(peer, it) }
        val burnIds = msgs.filter { it.burn > 0 }.map { it.id }.toSet()
        if (burnIds.isEmpty()) return@LaunchedEffect
        // markSeen 就地改存储，不触发重载——UI 手里的 Msg 还是 seenAt=0 的旧
        // 对象，角标会一直显示"未开始"。首次循环发现存储里的 seenAt 已生效
        // 就 bump 一次，让列表换上带 seenAt 的新对象。
        var seenSynced = msgs.all { it.seenAt > 0 }
        while (true) {
            delay(1_000)
            burnTick++
            Repo.sweepExpired(peer)
            val storeMsgs = withContext(Dispatchers.IO) { Repo.messagesOf(peer) }
            val stillThere = storeMsgs.any { it.id in burnIds }
            if (!stillThere) {
                localBump++
                return@LaunchedEffect
            }
            if (!seenSynced) {
                seenSynced = storeMsgs.all { it.burn <= 0 || it.seenAt > 0 }
                if (seenSynced) localBump++
            }
        }
    }

    // 核对状态跟着对话框开关一起变：关掉对话框时重新读一次（用户可能刚点了"我已核对"）
    LaunchedEffect(peer, showSafety) {
        verifiedBadge = withContext(Dispatchers.IO) { Repo.isVerified(peer) }
    }

    // 错误提示不该长驻：像"说话时间太短"这种挂一整屏很碍眼（模拟器上截到过）。
    // 4 秒够看清，又不至于让用户以为界面卡住了。
    LaunchedEffect(err) {
        if (err != null) {
            delay(4_000)
            err = null
        }
    }

    // 多选相关对话框。抽成 lambda 是因为"多选模式"那条分支会提前 return@Column——
    // 放在后面的对话框根本不会被渲染，表现为"删除按钮点了没反应"（实测踩到）。
    val batchDialogs: @Composable () -> Unit = {
        // 批量删除确认（本地删除，不通知对方）
        delMsgs?.let { ids ->
            AlertDialog(
                onDismissRequest = { delMsgs = null },
                title = { Text(stringResource(R.string.del_msgs_title, ids.size)) },
                text = { Text(stringResource(R.string.del_msgs_body)) },
                confirmButton = {
                    TextButton(onClick = {
                        val toDelete = ids
                        delMsgs = null
                        busy = true
                        kotlinx.coroutines.MainScope().launch {
                            withContext(Dispatchers.IO) { Repo.deleteMessages(peer, toDelete) }
                            busy = false
                            selecting = false
                            selected = emptySet()
                            localBump++
                        }
                    }) { Text(stringResource(R.string.action_delete)) }
                },
                dismissButton = { TextButton(onClick = { delMsgs = null }) { Text(stringResource(R.string.action_cancel)) } }
            )
        }

        // 转发：选一个联系人（只列已有联系人，陌生人没有身份密钥发不了）
        if (forwardOpen) {
            val targets = remember(forwardOpen) { Repo.contacts().filter { it.dh != peer } }
            AlertDialog(
                onDismissRequest = { forwardOpen = false },
                title = { Text(stringResource(R.string.forward_title, selected.size)) },
                text = {
                    Column {
                        if (targets.isEmpty()) {
                            Text(stringResource(R.string.forward_empty), style = MaterialTheme.typography.bodySmall)
                        }
                        targets.forEach { c ->
                            MenuRow(c.display) {
                                val ids = selected
                                forwardOpen = false
                                busy = true
                                kotlinx.coroutines.MainScope().launch {
                                    val list = msgs.filter { it.id in ids }
                                    val r = withContext(Dispatchers.IO) { Repo.forward(c.dh, list) }
                                    busy = false
                                    selecting = false
                                    selected = emptySet()
                                    err = if (r.ok) ctx.getString(R.string.forwarded_n, r.data?.first ?: 0) else errText(ctx, r)
                                }
                            }
                        }
                    }
                },
                confirmButton = { TextButton(onClick = { forwardOpen = false }) { Text(stringResource(R.string.action_cancel)) } }
            )
        }
    }

    Column(Modifier.fillMaxSize().background(LocalSkin.current.bg)) {
        if (selecting) {
            SelectionBar(
                count = selected.size,
                onCancel = { selecting = false; selected = emptySet() },
            )
        } else {
            ChatTopBar(
                peer = peer,
                verified = verifiedBadge,
                canCall = isContact,
                panicCode = panicCode,
                onBack = onBack,
                onSafety = { showSafety = true },
                onCall = { video -> onStartCall(peer, video) },
                onPanic = { showPanic = true },
            )
        }

        Box(Modifier.weight(1f).fillMaxWidth()) {
            if (msgs.isEmpty()) {
                Text(
                    stringResource(R.string.chat_empty),
                    style = MaterialTheme.typography.bodySmall,
                    modifier = Modifier.align(Alignment.Center)
                )
            }
            LazyColumn(
                Modifier.fillMaxSize().padding(horizontal = 12.dp, vertical = 10.dp),
                verticalArrangement = Arrangement.spacedBy(10.dp),
                state = listState,
            ) {
                itemsIndexed(msgs) { idx, m ->
                    // 读 burnTick：让焚毁倒计时气泡每秒重组一次（tick 的唯一用途）
                    @Suppress("UNUSED_EXPRESSION") burnTick
                    val nowSecs = System.currentTimeMillis() / 1000
                    // 相邻消息间隔超过 5 分钟时插入时间戳
                    val prev = msgs.getOrNull(idx - 1)
                    val needTime = prev == null || (m.ts - prev.ts) > 300
                    Column {
                        if (needTime) {
                            Text(
                                formatTs(m.ts),
                                fontSize = 12.sp,
                                color = skin.subText,
                                modifier = Modifier.fillMaxWidth().padding(vertical = 6.dp),
                                textAlign = TextAlign.Center
                            )
                        }
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            // 多选模式：勾选圈放在气泡外侧，不拦截气泡自己的点击（文件/图片仍能正常打开）
                            if (selecting) {
                                Box(
                                    Modifier.size(24.dp)
                                        .background(
                                            if (m.id in selected) skin.accent else skin.field,
                                            CircleShape
                                        )
                                        .clickable {
                                            selected = if (m.id in selected) selected - m.id else selected + m.id
                                        },
                                    contentAlignment = Alignment.Center
                                ) {
                                    if (m.id in selected) {
                                        Text("✓", fontSize = 13.sp, color = skin.bubbleText)
                                    }
                                }
                                Spacer(Modifier.width(8.dp))
                            }
                            Box(Modifier.weight(1f)) {
                                Bubble(
                                    m,
                                    nowSecs = nowSecs,
                                    onFile = { fb, mm -> fileMenu = fb; fileMenuMsg = mm },
                                    onMedia = { mv -> mediaView = mv },
                                    onResend = { failed -> resendFailed(failed) },
                                    onRecall = { mm -> recallMsg(mm) },
                                    onDelete = { mm -> delMsgs = setOf(mm.id) },
                                    onSelect = { mm ->
                                        selecting = true
                                        selected = setOf(mm.id)
                                    },
                                )
                            }
                        }
                    }
                }
                // 正在接收的文件：真实进度条（收齐后消失，出现文件气泡）
                val rxFiles = Repo.incomingFiles().filter { it.who == peer }
                if (rxFiles.isNotEmpty()) {
                    // 读一下 tick 让进度更新能触发重组
                    @Suppress("UNUSED_VARIABLE")
                    val tick = Repo.fileRxTick
                    item(key = "file-rx") {
                        Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                            rxFiles.forEach { f -> RxProgress(f) }
                        }
                    }
                }
            }
        }

        err?.let {
            Text(
                it,
                color = MaterialTheme.colorScheme.error,
                modifier = Modifier.padding(horizontal = 12.dp)
            )
        }

        fileSending?.let { tx ->
            val pct = if (tx.total <= 0) 0 else (tx.sent * 100 / tx.total).toInt().coerceIn(0, 100)
            Column(Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 4.dp)) {
                Text(
                    txLabel(tx, pct),
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant
                )
                LinearProgressIndicator(
                    progress = { pct / 100f },
                    modifier = Modifier.fillMaxWidth().padding(top = 4.dp)
                )
            }
        }

        // 录音中：输入栏上方一条实时反馈（计时 + 电平波形）。
        // 不做遮住聊天记录的浮层——手指这会儿正按着麦克风，能看见自己在录就够了。
        if (recording) {
            RecordingBar(recMs, recLevels, recCancelling)
        }

        // 陌生人会话：对方还没进通讯录，Rust 侧 find_contact 会直接失败，
        // 所以这里不给输入框，改成一个"加为联系人"的出口——否则用户输完字
        // 点发送只会看到"找不到联系人"，完全不知道下一步该干嘛。
        if (!isContact) {
            Column(
                Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 12.dp),
                horizontalAlignment = Alignment.CenterHorizontally
            ) {
                Text(
                    if (hasCard) stringResource(R.string.stranger_add_hint)
                    else stringResource(R.string.stranger_no_clue),
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    textAlign = TextAlign.Center
                )
                if (hasCard) {
                    Button(
                        enabled = !busy,
                        modifier = Modifier.padding(top = 10.dp),
                        onClick = {
                            busy = true; err = null
                            kotlinx.coroutines.MainScope().launch {
                                val r = withContext(Dispatchers.IO) { Repo.addPeer(peer) }
                                busy = false
                                if (r.ok) {
                                    // 会话键是 dh，加好友后键不变，只是显示名从"陌生人 xxxx"
                                    // 变成对方昵称——原地刷新即可，不需要切会话
                                    isContact = true
                                    err = null
                                    localBump++
                                    onPeerRenamed(peer)
                                } else err = errText(ctx, r)
                            }
                        }
                    ) { Text(stringResource(R.string.stranger_add_reply)) }
                }
            }
            return@Column
        }

        HorizontalDivider(color = skin.divider, thickness = 0.5.dp)

        // 多选模式：输入栏整体换成批量操作条（删除/转发/复制）
        if (selecting) {
            Row(
                Modifier.fillMaxWidth().background(skin.bar).padding(horizontal = 8.dp, vertical = 6.dp),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.SpaceEvenly,
            ) {
                TextButton(
                    enabled = selected.isNotEmpty() && !busy,
                    onClick = { delMsgs = selected }
                ) { Text(stringResource(R.string.action_delete)) }
                TextButton(
                    enabled = selected.isNotEmpty() && !busy,
                    onClick = { forwardOpen = true }
                ) { Text(stringResource(R.string.action_forward)) }
                TextButton(
                    enabled = selected.isNotEmpty(),
                    onClick = {
                        val text = msgs.filter { it.id in selected && it.kind == "text" }
                            .joinToString("\n") { it.text }
                        if (text.isNotBlank()) {
                            clipForCopy.setText(AnnotatedString(text))
                            err = ctx.getString(R.string.copied_n, selected.size)
                        } else err = ctx.getString(R.string.copied_no_text)
                    }
                ) { Text(stringResource(R.string.action_copy)) }
            }
            batchDialogs()
            return@Column
        }

        Row(
            Modifier.fillMaxWidth().background(skin.bar).padding(horizontal = 8.dp, vertical = 6.dp),
            verticalAlignment = Alignment.CenterVertically
        ) {
            Spacer(Modifier.width(2.dp))
            // 按住说话：按下就开始录，松手发出去，上滑取消。
            // 和微信一致，不需要先切"语音模式"——多一步切换就多一次误操作。
            VoiceHoldButton(
                enabled = fileSending == null && !busy,
                onDown = {
                    when {
                        // 通话中麦克风被 WebRTC 占着，硬开录音会失败或录出一段静音
                        CallManager.ui.state != "idle" -> {
                            err = ctx.getString(R.string.voice_play_during_call)
                            false
                        }
                        ContextCompat.checkSelfPermission(ctx, Manifest.permission.RECORD_AUDIO)
                            != PackageManager.PERMISSION_GRANTED -> {
                            recPerm.launch(Manifest.permission.RECORD_AUDIO)
                            false
                        }
                        else -> {
                            val e = VoiceRecorder.start(ctx)
                            if (e != null) {
                                err = when (e.kind) {
                                    VoiceRecorder.StartError.Kind.ALREADY_RECORDING ->
                                        ctx.getString(R.string.err_rec_already)
                                    VoiceRecorder.StartError.Kind.OPEN_FAILED ->
                                        ctx.getString(R.string.err_rec_open_failed, e.detail ?: "")
                                }
                                false
                            } else {
                                err = null
                                recMs = 0
                                recLevels = emptyList()
                                recCancelling = false
                                recording = true
                                true
                            }
                        }
                    }
                },
                onDrag = { recCancelling = it },
                onUp = { cancelled -> finishRecording(!cancelled) },
            )
            Spacer(Modifier.width(8.dp))
            // 输入框自己画：Material 的 TextField 有 56dp 最小高度和描边，
            // 放在一行里会把整个输入栏顶得很高；IM 这条栏通常只有 40dp 上下。
            BasicTextField(
                value = input,
                onValueChange = { input = it },
                modifier = Modifier
                    .weight(1f)
                    .background(skin.field, RoundedCornerShape(4.dp))
                    .padding(horizontal = 10.dp, vertical = 8.dp),
                textStyle = TextStyle(fontSize = 16.sp, color = skin.bubbleText),
                maxLines = 4,
                cursorBrush = SolidColor(skin.accent),
                decorationBox = { inner ->
                    Box(Modifier.fillMaxWidth().heightIn(min = 20.dp)) {
                        if (input.isEmpty()) {
                            Text(stringResource(R.string.input_hint), fontSize = 16.sp, color = skin.placeholder)
                        }
                        inner()
                    }
                },
            )
            Spacer(Modifier.width(8.dp))
            // 有字才出现"发送"，没字的位置是"+"：这是 IM 输入栏最省地方的做法，
            // 常驻一个发送按钮会让输入框永远少 60dp 宽度
            if (input.isBlank()) {
                Box(
                    Modifier.size(40.dp).clip(CircleShape).background(skin.chip)
                        .clickable(enabled = fileSending == null) { attachMenu = true },
                    contentAlignment = Alignment.Center,
                ) { Text("+", fontSize = 24.sp, color = skin.icon) }
            } else {
                Box(
                    Modifier.height(36.dp).widthIn(min = 58.dp)
                        .background(skin.accent, RoundedCornerShape(4.dp))
                        .clickable(enabled = !busy) {
                            val text = input
                            // 暗号命中：消息**不发出**（不进对方设备、不进中继日志），
                            // 先弹算术码，答对才执行清除
                            if (panicCode != null && Panic.matches(text, panicCode)) {
                                err = null
                                showPanic = true
                            } else {
                                busy = true; err = null
                                kotlinx.coroutines.MainScope().launch {
                                    val r = withContext(Dispatchers.IO) { Repo.send(peer, text, burn = burnOpt) }
                                    if (r.ok) {
                                        input = ""
                                        // 自己发的消息一定要滚到底，哪怕刚才在翻历史
                                        stickBottom = true
                                        localBump++
                                        burnOpt = 0   // 按条生效：发完复位
                                    } else {
                                        // 失败也要刷新：失败的消息已经落盘（failed=true），
                                        // 不 bump 的话红色感叹号要等重进会话才出现（模拟器实测踩到）
                                        err = errText(ctx, r)
                                        localBump++
                                    }
                                    busy = false
                                }
                            }
                        },
                    contentAlignment = Alignment.Center,
                ) {
                    Text(stringResource(R.string.action_send), fontSize = 14.sp, color = skin.onAccent, fontWeight = FontWeight.Medium)
                }
            }
        }

        if (showSafety) {
            SafetyDialog(peer, verifiedBadge) { verified ->
                verifiedBadge = verified
                showSafety = false
            }
        }

        // 紧急清除：算术码验证通过后清双方，杀进程重启
        if (showPanic && panicCode != null) {
            PanicDialog(code = panicCode, onConfirmed = { showPanic = false; executePanicNow() }) {
                showPanic = false
            }
        }

        // "+" 面板：拍照 / 图片 / 视频 / 文件。
        // 从底部推上来而不是弹居中的 AlertDialog —— 手指本来就停在屏幕下方，
        // 居中弹窗要往上够一次，而且"选类型"这种动作在 IM 里习惯是底部面板。
        if (attachMenu) {
            ModalBottomSheet(
                onDismissRequest = { attachMenu = false },
                sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true),
                containerColor = skin.sheet,
            ) {
                Row(
                    // 四个格子用 SpaceEvenly 均分：固定间距在 360dp 窄屏上会把"文件"挤出去
                    Modifier.fillMaxWidth().padding(horizontal = 8.dp, vertical = 8.dp),
                    horizontalArrangement = Arrangement.SpaceEvenly,
                ) {
                    AttachTile(stringResource(R.string.attach_camera), "📷") {
                        attachMenu = false
                        if (ContextCompat.checkSelfPermission(ctx, Manifest.permission.CAMERA) ==
                            PackageManager.PERMISSION_GRANTED
                        ) shootPhoto() else askCamera.launch(Manifest.permission.CAMERA)
                    }
                    AttachTile(stringResource(R.string.attach_image), "🖼") {
                        attachMenu = false
                        pickImage.launch(
                            PickVisualMediaRequest(ActivityResultContracts.PickVisualMedia.ImageOnly)
                        )
                    }
                    AttachTile(stringResource(R.string.attach_video), "🎬") {
                        attachMenu = false
                        pickVideo.launch(
                            PickVisualMediaRequest(ActivityResultContracts.PickVisualMedia.VideoOnly)
                        )
                    }
                    AttachTile(stringResource(R.string.attach_file), "📄") {
                        attachMenu = false
                        pickFile.launch(arrayOf("*/*"))
                    }
                }
                // 阅后即焚开关：点击在 关 → 30 秒 → 5 分钟 间循环。
                // 按条生效，选中后下一条消息（任何类型）带焚毁标记，发完自动复位。
                // 刻意不关面板：切完档位能立刻看到状态变化，再点别的类型接着发
                Row(
                    Modifier.fillMaxWidth().padding(horizontal = 20.dp, vertical = 8.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Text(stringResource(R.string.burn_title), fontSize = 14.sp, color = skin.icon)
                    Spacer(Modifier.width(12.dp))
                    Burn.OPTIONS.forEach { opt ->
                        val selected = burnOpt == opt
                        Text(
                            burnLabel(opt),
                            fontSize = 13.sp,
                            color = if (selected) skin.onAccent else skin.subText,
                            fontWeight = if (selected) FontWeight.Medium else FontWeight.Normal,
                            modifier = Modifier
                                .clip(RoundedCornerShape(6.dp))
                                .background(if (selected) skin.accent else skin.chip)
                                .clickable { burnOpt = opt }
                                .padding(horizontal = 12.dp, vertical = 6.dp),
                        )
                        Spacer(Modifier.width(8.dp))
                    }
                }
                Text(
                    stringResource(R.string.burn_hint),
                    fontSize = 12.sp,
                    lineHeight = 17.sp,
                    color = skin.subText,
                    modifier = Modifier.padding(horizontal = 20.dp),
                )
                Spacer(Modifier.height(20.dp))
            }
        }

        // 全屏看图 / 内置播放器。关掉时把引用清掉，否则下次重进页面会自动弹出来
        mediaView?.let { mv ->
            if (mv.video) {
                VideoPlayerDialog(mv.path, mv.name) { mediaView = null }
            } else {
                ImageViewerDialog(mv.path) { mediaView = null }
            }
        }

        // 文件菜单。点气泡才出现，不占聊天页面空间。
        batchDialogs()

        fileMenu?.let { fb ->
            val has = fb.file != null
            val menuMsg = fileMenuMsg
            AlertDialog(
                onDismissRequest = { fileMenu = null; fileMenuMsg = null },
                title = { Text(fb.name, maxLines = 2, overflow = TextOverflow.Ellipsis) },
                text = {
                    Column {
                        Text(formatSize(fb.size), style = MaterialTheme.typography.bodySmall)
                        fb.saved?.let {
                            Text(
                                stringResource(R.string.file_already_at, it),
                                style = MaterialTheme.typography.bodySmall,
                                color = MaterialTheme.colorScheme.onSurfaceVariant
                            )
                        }
                        if (!has) {
                            Text(
                                stringResource(R.string.file_gone_hint),
                                style = MaterialTheme.typography.bodySmall,
                                color = MaterialTheme.colorScheme.onSurfaceVariant
                            )
                        }
                        Spacer(Modifier.height(6.dp))
                        if (has) {
                            MenuRow(stringResource(R.string.file_open)) {
                                fileMenu = null; fileMenuMsg = null
                                openDownloadedFile(ctx, fb.path!!)
                            }
                            MenuRow(
                                if (fb.saved != null) stringResource(R.string.file_save_again)
                                else stringResource(R.string.file_save_device)
                            ) {
                                fileMenu = null; fileMenuMsg = null
                                saveFile(fb)
                            }
                            MenuRow(stringResource(R.string.file_save_as)) {
                                fileMenu = null; fileMenuMsg = null
                                pendingSave = fb
                                saveAs.launch(fb.name)
                            }
                            MenuRow(stringResource(R.string.action_share)) {
                                fileMenu = null; fileMenuMsg = null
                                FileSaver.shareIntent(ctx, fb.file!!, fb.name)?.let { ctx.startActivity(it) }
                            }
                        }
                        // 撤回：自己发的、带 rid、在 10 分钟窗口内的文件/图片/视频
                        if (menuMsg != null && Repo.canRecall(menuMsg)) {
                            MenuRow(stringResource(R.string.menu_recall)) {
                                fileMenu = null; fileMenuMsg = null
                                recallMsg(menuMsg)
                            }
                        }
                        if (menuMsg != null) {
                            val mm = menuMsg
                            MenuRow(stringResource(R.string.menu_multiselect)) {
                                fileMenu = null; fileMenuMsg = null
                                selecting = true
                                selected = setOf(mm.id)
                            }
                            MenuRow(stringResource(R.string.action_delete)) {
                                fileMenu = null; fileMenuMsg = null
                                delMsgs = setOf(mm.id)
                            }
                        }
                    }
                },
                confirmButton = {
                    TextButton(onClick = { fileMenu = null; fileMenuMsg = null }) {
                        Text(stringResource(R.string.action_close))
                    }
                }
            )
        }
    }
}

/**
 * 紧急清除确认对话框：算术码 + 明确的不可逆警告。
 *
 * 三个防线：暗号本身（按身份派生、不常用）、算术码（防手滑/防他人拿手机误触）、
 * 明确告知后果（清双方、不可恢复）。答错 3 次自动取消。
 */
@Composable
private fun PanicDialog(code: String, onConfirmed: () -> Unit, onDismiss: () -> Unit) {
    val skin = LocalSkin.current
    // "得数不对"那句在点击回调里（不是组合期），stringResource 用不了，得拿 Context
    val ctx = androidx.compose.ui.platform.LocalContext.current
    val rnd = remember { kotlin.random.Random.Default }
    var quiz by remember { mutableStateOf(Panic.quiz(rnd)) }
    var answer by remember { mutableStateOf("") }
    var attempts by remember { mutableStateOf(0) }
    var wrong by remember { mutableStateOf<String?>(null) }

    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(stringResource(R.string.panic_title), color = MaterialTheme.colorScheme.error) },
        text = {
            Column {
                Text(
                    stringResource(R.string.panic_body, code),
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Spacer(Modifier.height(12.dp))
                Text(
                    quiz.first,
                    fontSize = 20.sp,
                    fontWeight = FontWeight.Medium,
                    modifier = Modifier.fillMaxWidth(),
                    textAlign = TextAlign.Center,
                )
                Spacer(Modifier.height(8.dp))
                OutlinedTextField(
                    value = answer,
                    onValueChange = { answer = it.filter { ch -> ch.isDigit() }.take(4) },
                    keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                    placeholder = { Text(stringResource(R.string.panic_answer_hint), fontSize = 14.sp) },
                )
                wrong?.let {
                    Spacer(Modifier.height(6.dp))
                    Text(it, color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodySmall)
                }
                Spacer(Modifier.height(6.dp))
                Text(
                    stringResource(R.string.panic_attempts_left, Panic.MAX_ATTEMPTS - attempts),
                    style = MaterialTheme.typography.bodySmall,
                    color = skin.subText,
                )
            }
        },
        confirmButton = {
            TextButton(
                onClick = {
                    val n = answer.trim().toIntOrNull()
                    if (n != null && n == quiz.second) {
                        onConfirmed()
                    } else {
                        attempts += 1
                        if (attempts >= Panic.MAX_ATTEMPTS) {
                            onDismiss()
                        } else {
                            wrong = ctx.getString(R.string.panic_wrong)
                            quiz = Panic.quiz(rnd)
                            answer = ""
                        }
                    }
                }
            ) { Text(stringResource(R.string.panic_confirm), color = MaterialTheme.colorScheme.error) }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) { Text(stringResource(R.string.action_cancel)) }
        }
    )
}

/**
 * 安全码对话框。
 *
 * 为什么值得做：这版协议里对方身份只靠"第一次添加时的邀请码"锚定，
 * 之后换设备/被劫持都没有任何提示。安全码让两个人在别的渠道（当面、电话）
 * 念一串数字，就能确认中间没坐第三个人——这是纯软件唯一能做的带外验证。
 */
@Composable
private fun SafetyDialog(peer: String, verifiedNow: Boolean, onClose: (Boolean) -> Unit) {
    val ctx = androidx.compose.ui.platform.LocalContext.current
    val clipboard = LocalClipboardManager.current
    // 打开时算一次。会话没卡片（旧客户端/还没收到过消息）时算不出来
    val code = remember(peer) { Repo.safetyCodeOf(peer) }
    var verified by remember(peer) { mutableStateOf(verifiedNow) }

    AlertDialog(
        onDismissRequest = { onClose(verified) },
        title = { Text(stringResource(R.string.safety_title, peer)) },
        text = {
            Column {
                Text(
                    stringResource(R.string.safety_hint),
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant
                )
                Spacer(Modifier.height(12.dp))
                if (code == null) {
                    Text(
                        stringResource(R.string.safety_no_key),
                        style = MaterialTheme.typography.bodyMedium
                    )
                } else {
                    code.chunked(20).forEach { line ->
                        Text(
                            line.chunked(5).joinToString(" "),
                            style = MaterialTheme.typography.titleMedium,
                            fontFamily = FontFamily.Monospace,
                            textAlign = TextAlign.Center,
                            modifier = Modifier.fillMaxWidth().padding(vertical = 2.dp)
                        )
                    }
                    if (verified) {
                        Spacer(Modifier.height(8.dp))
                        Text(
                            stringResource(R.string.safety_verified),
                            style = MaterialTheme.typography.labelLarge,
                            color = MaterialTheme.colorScheme.primary
                        )
                    }
                }
            }
        },
        confirmButton = {
            TextButton(onClick = { onClose(verified) }) { Text(stringResource(R.string.action_close)) }
        },
        dismissButton = {
            Row {
                if (code != null) {
                    TextButton(onClick = {
                        clipboard.setText(AnnotatedString(code!!.chunked(5).joinToString(" ")))
                        android.widget.Toast
                            .makeText(ctx, ctx.getString(R.string.safety_copied), android.widget.Toast.LENGTH_SHORT)
                            .show()
                    }) { Text(stringResource(R.string.action_copy)) }
                    TextButton(onClick = {
                        val next = !verified
                        // 只在"要标成已核对"时才写码；取消就清掉
                        if (next) Repo.setVerified(peer, true) else Repo.setVerified(peer, false)
                        verified = next
                    }) {
                        Text(
                            if (verified) stringResource(R.string.safety_unverify)
                            else stringResource(R.string.safety_verify)
                        )
                    }
                }
            }
        }
    )
}

/** 会话列表里的通话记录一行文案；与 [callLogText] 共用同一批资源 */
@Composable
private fun callPreviewText(p: Preview.Call): String {
    val ctx = androidx.compose.ui.platform.LocalContext.current
    val mmss = "%02d:%02d".format(p.durSec / 60, p.durSec % 60)
    return when (p.reason) {
        "declined" -> ctx.getString(
            if (p.fromPeer) R.string.call_end_declined else R.string.calllog_declined_me
        )
        "busy" -> ctx.getString(R.string.call_end_busy)
        "no_answer" -> ctx.getString(
            if (p.fromPeer) R.string.call_end_no_answer else R.string.calllog_no_answer_me
        )
        "missed" -> ctx.getString(R.string.call_end_missed)
        "local_ended" ->
            if (p.durSec > 0) ctx.getString(R.string.calllog_ongoing, mmss)
            else ctx.getString(R.string.calllog_cancelled)
        else ->
            if (p.durSec > 0) ctx.getString(R.string.calllog_ongoing, mmss)
            else ctx.getString(R.string.calllog_ended)
    }
}

/**
 * 通话记录的显示文案。
 *
 * 新版本存的是结构化 JSON（原因 + 时长），按当前语言拼句子；
 * 旧版本存的是拼好的中文，解析不了就原样显示（老数据兼容）。
 */
@Composable
private fun callLogText(m: Msg): String {
    val ctx = androidx.compose.ui.platform.LocalContext.current
    val o = runCatching { org.json.JSONObject(m.text) }.getOrNull()
        ?: return m.text
    val reason = o.optString("r")
    val dur = o.optLong("d", 0L)
    val mmss = "%02d:%02d".format(dur / 60, dur % 60)
    // 通话记录是本机写的（outgoing 恒为 true），方向信息在 reason 里
    val fromPeer = o.optBoolean("fp")
    return when (reason) {
        "declined" -> ctx.getString(
            if (fromPeer) R.string.call_end_declined else R.string.calllog_declined_me
        )
        "busy" -> ctx.getString(R.string.call_end_busy)
        "no_answer" -> ctx.getString(
            if (fromPeer) R.string.call_end_no_answer else R.string.calllog_no_answer_me
        )
        "missed" -> ctx.getString(R.string.call_end_missed)
        "local_ended" ->
            if (dur > 0) ctx.getString(R.string.calllog_ongoing, mmss)
            else ctx.getString(R.string.calllog_cancelled)
        else ->
            if (dur > 0) ctx.getString(R.string.calllog_ongoing, mmss)
            else ctx.getString(R.string.calllog_ended)
    }
}

/**
 * 时间戳显示。日期格式随语言变（中文"10月23日 14:53"，英文"Oct 23, 14:53"），
 * 所以格式模板走资源，Locale 也用系统当前的。
 */
@Composable
private fun formatTs(ts: Long): String {
    val d = Date(ts * 1000)
    val now = System.currentTimeMillis() / 1000
    val fmt = if (now - ts > 86_400) stringResource(R.string.ts_fmt_day)
    else stringResource(R.string.ts_fmt_time)
    val locale = androidx.compose.ui.platform.LocalConfiguration.current.locales[0]
    return SimpleDateFormat(fmt, locale).format(d)
}

/**
 * 阅后即焚倒计时角标，贴在任意气泡（文字/图片/视频/文件/语音）旁边。
 *
 * 早先只有文字气泡有这个角标，媒体类"静默焚毁"——真机实测反馈体验割裂：
 * 用户看不出带焚毁的图片和普通图片有什么区别，更不知道它什么时候会消失。
 * 未展示（seenAt=0）时显示全额档位（30s/5m），展示后按秒倒计。
 */
@Composable
private fun BurnBadge(m: Msg, nowSecs: Long, bottomPad: Boolean) {
    if (m.burn <= 0) return
    // 没展示过（seenAt=0，比如刚发出的那几秒、或从通知预览过）显示全额档位，
    // 展示后按秒倒计。与旧文字角标行为一致。
    val left = if (m.seenAt > 0) Burn.remaining(m.burn, m.seenAt, nowSecs) else m.burn
    Spacer(Modifier.width(4.dp))
    Text(
        "${left}s",
        fontSize = 11.sp,
        color = MaterialTheme.colorScheme.error,
        modifier = Modifier.padding(bottom = if (bottomPad) 4.dp else 0.dp),
    )
}

@Composable
private fun Bubble(
    m: Msg,
    /** 当前时刻（秒），由列表层每秒刷新一次——焚毁倒计时要用它重算剩余时间 */
    nowSecs: Long = 0,
    onFile: (FileBubble, Msg) -> Unit = { _, _ -> },
    onMedia: (MediaBubble) -> Unit = {},
    /** 点红色感叹号（或菜单里的"重发"）时回调，只有发送失败的文字消息会有 */
    onResend: (Msg) -> Unit = {},
    /** 长按菜单里点"撤回"时回调 */
    onRecall: (Msg) -> Unit = {},
    /** 长按菜单里点"删除"时回调（只删本机） */
    onDelete: (Msg) -> Unit = {},
    /** 长按菜单里点"多选"时回调 */
    onSelect: (Msg) -> Unit = {},
) {
    val skin = LocalSkin.current
    // 通话记录、撤回记录居中显示，不占左右气泡的位置
    if (m.kind == "call" || m.kind == "recall") {
        Row(
            Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.Center
        ) {
            Text(
                when {
                    m.kind == "recall" -> stringResource(
                        if (m.outgoing) R.string.recall_note_mine else R.string.recall_note_peer
                    )
                    m.kind == "call" -> callLogText(m)
                    else -> m.text
                },
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant
            )
        }
        return
    }
    // 图片/视频：直接显示缩略图，点一下在 App 内全屏看 / 播放。
    // 长按才出菜单（保存到手机、另存为、分享），和微信的习惯一致。
    if (m.kind == "image" || m.kind == "video") {
        val meta = Media.parse(m.text)
        val isVideo = m.kind == "video"
        val path = meta?.path?.takeIf { java.io.File(it).exists() }
        val cfg = LocalConfiguration.current
        // 图片气泡最多占屏宽 62%、屏高 42%：再大就把整屏都占了，
        // 一屏看不到几条消息；比例由 Media.bubbleDims 保证不变形。
        val (bw, bh) = Media.bubbleDims(
            cfg.screenWidthDp * 0.62f,
            cfg.screenHeightDp * 0.42f,
            meta?.w ?: 0,
            meta?.h ?: 0,
        )
        // 按显示尺寸的 2 倍解码，缩放/高密度屏都不糊
        val px = with(LocalDensity.current) {
            (kotlin.math.max(bw, bh).dp.toPx() * 2f).toInt()
        }.coerceIn(128, 1600)
        val bmp = rememberMediaBitmap(path, px, isVideo)
        val ctx = androidx.compose.ui.platform.LocalContext.current
        // 长按菜单里用的默认名：onLongPress 是手势回调，不是组合期，
        // 里面调不了 stringResource，所以提前算好
        val defaultMediaName = if (isVideo) stringResource(R.string.media_kind_video)
        else stringResource(R.string.media_image)

        Row(
            Modifier.fillMaxWidth(),
            horizontalArrangement = if (m.outgoing) Arrangement.End else Arrangement.Start
        ) {
            Box(
                Modifier
                    .width(bw.dp)
                    .height(bh.dp)
                    .clip(RoundedCornerShape(SkinDims.image))
                    .background(if (m.outgoing) skin.outgoing else skin.incoming)
                    .pointerInput(path, isVideo) {
                        detectTapGestures(
                            onTap = {
                                if (path == null) {
                                    android.widget.Toast.makeText(
                                        ctx,
                                        if (isVideo) ctx.getString(R.string.media_gone_video)
                                        else ctx.getString(R.string.media_gone_image),
                                        android.widget.Toast.LENGTH_SHORT
                                    ).show()
                                } else {
                                    onMedia(MediaBubble(path, isVideo, meta?.name ?: ""))
                                }
                            },
                            // 长按复用文件菜单：图片/视频也能保存到手机、另存为、分享
                            onLongPress = {
                                onFile(
                                    FileBubble(
                                        name = meta?.name?.takeIf { it.isNotBlank() }
                                            ?: defaultMediaName,
                                        size = meta?.size ?: 0L,
                                        path = path,
                                        saved = meta?.saved,
                                    ),
                                    m,
                                )
                            },
                        )
                    },
                contentAlignment = Alignment.Center,
            ) {
                if (bmp != null) {
                    Image(
                        bitmap = bmp.asImageBitmap(),
                        contentDescription = if (isVideo) stringResource(R.string.media_kind_video)
                        else stringResource(R.string.media_image),
                        contentScale = ContentScale.Crop,
                        modifier = Modifier.fillMaxSize(),
                    )
                } else {
                    Text(
                        if (path == null) stringResource(R.string.media_not_local)
                        else stringResource(R.string.media_loading),
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
                if (isVideo) {
                    // 播放角标：没有它的话，一张首帧缩略图看起来就是一张图片
                    Box(
                        Modifier
                            .size(44.dp)
                            .background(Color.Black.copy(alpha = 0.42f), CircleShape),
                        contentAlignment = Alignment.Center,
                    ) { Text("▶", color = Color.White, fontSize = 18.sp) }
                    val dur = meta?.durMs ?: 0
                    if (dur > 0) {
                        Text(
                            Media.durLabel(dur),
                            color = Color.White,
                            fontSize = 11.sp,
                            modifier = Modifier
                                .align(Alignment.BottomStart)
                                .padding(6.dp)
                                .background(Color.Black.copy(alpha = 0.42f), RoundedCornerShape(6.dp))
                                .padding(horizontal = 6.dp, vertical = 2.dp),
                        )
                    }
                }
            }
            BurnBadge(m, nowSecs, bottomPad = false)
        }
        return
    }
    // 文件气泡：图标 + 文件名 + 大小。点一下弹菜单（打开 / 保存到手机 / 另存为 / 分享）
    if (m.kind == "file") {
        val v = runCatching { org.json.JSONObject(m.text) }.getOrNull()
        val name = v?.optString("name").takeIf { !it.isNullOrBlank() }
            ?: stringResource(R.string.file_default_name)
        val size = v?.optLong("size", 0) ?: 0L
        val path = v?.optString("path")?.takeIf { it.isNotBlank() }
        val saved = v?.optString("saved")?.takeIf { it.isNotBlank() }
        val canOpen = path != null && java.io.File(path).exists()
        // 这两行必须在组合期算好：buildString 块里不能调 stringResource
        val sizeLabel = formatSize(size)
        val stateLabel = when {
            saved != null -> stringResource(R.string.file_state_saved)
            canOpen -> stringResource(R.string.file_state_open)
            else -> stringResource(R.string.file_state_absent)
        }
        Row(
            Modifier.fillMaxWidth(),
            horizontalArrangement = if (m.outgoing) Arrangement.End else Arrangement.Start
        ) {
            val maxW = (LocalConfiguration.current.screenWidthDp * 0.78f).dp
            Row(
                Modifier
                    .widthIn(max = maxW)
                    // 文件消息两侧都用白底（深色下是深灰）：微信里自己发的文件也是白气泡，
                    // 靠左右对齐区分方向就够了，绿色留给纯文本
                    .background(skin.incoming, RoundedCornerShape(SkinDims.bubble))
                    // 始终可点：文件不在本机时点开也要能看到"为什么"，
                    // 而不是点了没反应让人以为界面坏了
                    .clickable { onFile(FileBubble(name, size, path, saved), m) }
                    .padding(horizontal = SkinDims.bubblePadH + 2.dp, vertical = SkinDims.bubblePadV + 2.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text("📄", fontSize = 20.sp)
                Spacer(Modifier.width(8.dp))
                Column {
                    Text(name, fontWeight = FontWeight.Medium, maxLines = 1,
                        overflow = TextOverflow.Ellipsis, color = skin.bubbleText)
                    Text(
                        "$sizeLabel · $stateLabel",
                        style = MaterialTheme.typography.bodySmall,
                        color = skin.subText
                    )
                }
            }
            BurnBadge(m, nowSecs, bottomPad = false)
        }
        return
    }
    // 语音气泡：点一下播放，再点一下停。刻意**不给**"保存到手机"——
    // 语音是聊天正文不是文件，落进「下载」只会变成一堆 voice_xxx.m4a 碎片。
    if (m.kind == "voice") {
        val meta = Voice.parse(m.text)
        val path = meta?.path?.takeIf { java.io.File(it).exists() }
        val dur = meta?.durMs ?: 0L
        val playing = path != null && VoicePlayer.playingPath == path
        val ctx = androidx.compose.ui.platform.LocalContext.current
        val maxW = (LocalConfiguration.current.screenWidthDp * 0.78f).dp
        Row(
            Modifier.fillMaxWidth(),
            horizontalArrangement = if (m.outgoing) Arrangement.End else Arrangement.Start
        ) {
            Row(
                Modifier
                    // 用 min 而不是固定 width：宽度由时长撑开，但不会窄到把秒数挤掉
                    .widthIn(min = Voice.bubbleWidthDp(dur).dp, max = maxW)
                    .background(
                        if (m.outgoing) skin.outgoing else skin.incoming,
                        RoundedCornerShape(SkinDims.bubble)
                    )
                    .clickable {
                        when {
                            path == null -> android.widget.Toast.makeText(
                                ctx, ctx.getString(R.string.voice_gone), android.widget.Toast.LENGTH_SHORT
                            ).show()
                            // 通话中播放会和听筒抢音频通道，先挡住
                            CallManager.ui.state != "idle" -> android.widget.Toast.makeText(
                                ctx, ctx.getString(R.string.voice_play_during_call), android.widget.Toast.LENGTH_SHORT
                            ).show()
                            else -> VoicePlayer.toggle(path)
                        }
                    }
                    .padding(horizontal = SkinDims.bubblePadH + 2.dp, vertical = SkinDims.bubblePadV + 2.dp),
                verticalAlignment = Alignment.CenterVertically
            ) {
                // 不做波形柱：气泡宽度本身已经编码了时长，再加一排柱子是两套信息打架，
                // 而且静止时那排灰柱子看起来像"还在加载"。
                Text(if (path == null) "⚠" else "🔊", fontSize = 14.sp)
                Spacer(Modifier.weight(1f))
                Text(
                    // 播放中显示剩余秒数，静止时显示总长
                    Voice.bubbleLabel(
                        if (playing) (dur - VoicePlayer.posMs).coerceAtLeast(0L) else dur
                    ),
                    style = MaterialTheme.typography.bodySmall,
                    color = if (playing) skin.accent else skin.bubbleText
                )
            }
            BurnBadge(m, nowSecs, bottomPad = false)
        }
        return
    }
    val ctx = androidx.compose.ui.platform.LocalContext.current
    val clipboard = LocalClipboardManager.current
    // 长按菜单的状态放在气泡自己身上：菜单是模态的（Popup 抢焦点），
    // 开着的时候用户滚不动列表，不会出现"状态跟着 item 回收丢掉"的情况
    var menu by remember { mutableStateOf(false) }
    // 红色感叹号只有"发送失败且还没重发成功"的文字消息才有；
    // 微信同款交互：点感叹号直接重发，不弹确认（失败的代价用户已经付过一次了）
    val failed = m.outgoing && m.failed
    Row(
        Modifier.fillMaxWidth(),
        horizontalArrangement = if (m.outgoing) Arrangement.End else Arrangement.Start,
        verticalAlignment = Alignment.Bottom,
    ) {
        if (failed) {
            // 感叹号排在气泡前面：End 对齐下它出现在气泡靠聊天区的一侧，
            // 不会把气泡顶离屏幕右缘
            Box(
                Modifier
                    .size(22.dp)
                    .background(MaterialTheme.colorScheme.error, CircleShape)
                    .clickable { onResend(m) },
                contentAlignment = Alignment.Center,
            ) { Text("!", color = Color.White, fontSize = 14.sp, fontWeight = FontWeight.Bold) }
            Spacer(Modifier.width(6.dp))
        }
        // 气泡最宽 78% 屏宽：长消息不该横着铺满整行
        val maxW = (LocalConfiguration.current.screenWidthDp * 0.78f).dp
        Box(contentAlignment = if (m.outgoing) Alignment.TopEnd else Alignment.TopStart) {
            Box(
                Modifier
                    .widthIn(max = maxW)
                    .background(
                        if (m.outgoing) skin.outgoing else skin.incoming,
                        RoundedCornerShape(SkinDims.bubble)
                    )
                    // 之前文字气泡没有任何手势，长按毫无反应 —— 想复制一段地址都做不到
                    .pointerInput(m.text) { detectTapGestures(onLongPress = { menu = true }) }
                    .padding(horizontal = SkinDims.bubblePadH + 2.dp, vertical = SkinDims.bubblePadV + 2.dp)
            ) {
                Text(
                    m.text,
                    fontSize = 16.sp,
                    lineHeight = 23.sp,
                    color = skin.bubbleText,
                )
            }
            DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
                DropdownMenuItem(
                    text = { Text(stringResource(R.string.action_copy)) },
                    onClick = {
                        menu = false
                        clipboard.setText(AnnotatedString(m.text))
                        android.widget.Toast
                            .makeText(ctx, ctx.getString(R.string.toast_copied), android.widget.Toast.LENGTH_SHORT)
                            .show()
                    },
                )
                DropdownMenuItem(
                    text = { Text(stringResource(R.string.action_share)) },
                    onClick = {
                        menu = false
                        val send = Intent(Intent.ACTION_SEND).apply {
                            type = "text/plain"
                            putExtra(Intent.EXTRA_TEXT, m.text)
                        }
                        ctx.startActivity(Intent.createChooser(send, ctx.getString(R.string.share_text_chooser)))
                    },
                )
                if (failed) {
                    DropdownMenuItem(
                        text = { Text(stringResource(R.string.action_resend)) },
                        onClick = {
                            menu = false
                            onResend(m)
                        },
                    )
                }
                // 撤回入口在菜单打开时才算窗口：挂着的菜单不会因为过了 10 分钟
                // 而自己刷新，但重新长按一次就会重新判定，足够了
                if (Repo.canRecall(m)) {
                    DropdownMenuItem(
                        text = { Text(stringResource(R.string.menu_recall)) },
                        onClick = {
                            menu = false
                            onRecall(m)
                        },
                    )
                }
                DropdownMenuItem(
                    text = { Text(stringResource(R.string.menu_multiselect)) },
                    onClick = {
                        menu = false
                        onSelect(m)
                    },
                )
                DropdownMenuItem(
                    text = { Text(stringResource(R.string.action_delete), color = MaterialTheme.colorScheme.error) },
                    onClick = {
                        menu = false
                        onDelete(m)
                    },
                )
            }
        }
        // 焚毁倒计时：贴在气泡旁边，按秒跳（BurnBadge 见顶部；文字气泡有底部对齐）
        BurnBadge(m, nowSecs, bottomPad = true)
    }
}

private fun formatSize(n: Long): String = when {
    n >= 1024 * 1024 -> "%.1f MB".format(n / 1024f / 1024f)
    n >= 1024 -> "%.1f KB".format(n / 1024f)
    else -> "$n B"
}

/** 录音中那一行：计时 + 实时电平 + 操作提示 */
@Composable
private fun RecordingBar(ms: Long, levels: List<Int>, cancelling: Boolean) {
    val tint = if (cancelling) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.primary
    Row(
        Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 6.dp),
        verticalAlignment = Alignment.CenterVertically
    ) {
        Text("🎤", fontSize = 16.sp)
        Spacer(Modifier.width(8.dp))
        Text(Voice.clock(ms), color = tint, fontWeight = FontWeight.Medium)
        Spacer(Modifier.width(10.dp))
        LevelBars(levels, tint, Modifier.weight(1f))
        Spacer(Modifier.width(10.dp))
        Text(
            if (cancelling) stringResource(R.string.rec_cancel_hint)
            else stringResource(R.string.rec_send_hint),
            style = MaterialTheme.typography.bodySmall,
            color = if (cancelling) tint else MaterialTheme.colorScheme.onSurfaceVariant
        )
    }
}

/**
 * 录音电平柱，右对齐、新数据在右端。
 *
 * 这是用户唯一能确认"麦克风真的在工作"的信号：真机上如果只显示计时，
 * 话筒被静音/被占用时用户完全看不出来，录完才发现是一段无声文件。
 */
@Composable
private fun LevelBars(levels: List<Int>, color: Color, modifier: Modifier = Modifier) {
    val shown = levels.takeLast(26)
    Row(
        modifier.height(26.dp),
        horizontalArrangement = Arrangement.spacedBy(2.dp, Alignment.End),
        verticalAlignment = Alignment.CenterVertically
    ) {
        repeat(26 - shown.size) {
            Box(
                Modifier.width(3.dp).height(3.dp)
                    .background(color.copy(alpha = 0.15f), RoundedCornerShape(2.dp))
            )
        }
        shown.forEach { a ->
            // 振幅跨两个数量级，取对数才看得出起伏；log(20001) 把 0..20000 归一到 0..1
            val norm = (kotlin.math.log10(1.0 + a) / kotlin.math.log10(20001.0))
                .toFloat().coerceIn(0f, 1f)
            Box(
                Modifier.width(3.dp).height((3 + norm * 23).dp)
                    .background(color, RoundedCornerShape(2.dp))
            )
        }
    }
}

/**
 * 按住说话按钮。
 *
 * 用原始指针事件而不是 detectTapGestures：要的是"按下的瞬间就开始录"，
 * 而不是等长按判定成立（那 500ms 会被吞掉，说话开头永远是半截）；
 * 同时要读手指移动的距离来判断是否取消。
 */
@Composable
private fun VoiceHoldButton(
    enabled: Boolean,
    onDown: () -> Boolean,
    onDrag: (cancelling: Boolean) -> Unit,
    onUp: (cancelled: Boolean) -> Unit,
) {
    val thresholdPx = with(LocalDensity.current) { Voice.CANCEL_DP.dp.toPx() }
    val skin = LocalSkin.current
    Surface(
        shape = CircleShape,
        color = if (enabled) skin.chip else skin.bg,
        modifier = Modifier
            .size(40.dp)
            .pointerInput(enabled, thresholdPx) {
                if (!enabled) return@pointerInput
                awaitEachGesture {
                    val down = awaitFirstDown(requireUnconsumed = false)
                    if (!onDown()) return@awaitEachGesture
                    var cancel = false
                    var released = false
                    try {
                        while (true) {
                            val ev = awaitPointerEvent()
                            val ch = ev.changes.firstOrNull { it.id == down.id } ?: break
                            cancel = Voice.shouldCancel(down.position.y - ch.position.y, thresholdPx)
                            onDrag(cancel)
                            if (!ch.pressed) {
                                released = true
                                break
                            }
                        }
                    } finally {
                        // 手势被系统打断（来电、切后台、上滑回桌面）也必须收尾，
                        // 否则麦克风会被这个已经不存在的手势一直占着
                        onUp(if (released) cancel else true)
                    }
                }
            }
    ) {
        Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
            Text("🎤", fontSize = 18.sp)
        }
    }
}

/** 正在发送的文件/语音/图片/视频进度 */
private data class TxFile(
    val name: String,
    val sent: Long,
    val total: Long,
    /** 语音不显示文件名和字节数，只说"发送语音 xx%" */
    val voice: Boolean = false,
    /** "image" / "video" / ""（普通文件）。图片压缩阶段 total 还是 0 */
    val media: String = "",
)

/**
 * 进度条上方那行字。total<=0 说明还在本地处理（压缩/探测），字节数没意义。
 *
 * 文案在 UI 层生成（而不是留在 TxFile 里）：显示文字必须随系统语言变，
 * data class 不该持有界面文案。
 */
@Composable
private fun txLabel(tx: TxFile, pct: Int): String = when {
    tx.total <= 0 && tx.media == "image" -> stringResource(R.string.tx_compress_image)
    tx.total <= 0 && tx.media == "video" -> stringResource(R.string.tx_read_video)
    tx.voice -> stringResource(R.string.tx_voice, pct)
    tx.media == "image" -> stringResource(R.string.tx_image, pct)
    tx.media == "video" -> stringResource(
        R.string.tx_video, pct, Media.sizeLabel(tx.sent), Media.sizeLabel(tx.total)
    )
    else -> stringResource(
        R.string.tx_file, tx.name, pct, Media.sizeLabel(tx.sent), Media.sizeLabel(tx.total)
    )
}

/** 聊天里一个文件气泡携带的信息，点开菜单时用 */
private data class FileBubble(
    val name: String,
    val size: Long,
    /** 本机副本路径（App 私有目录）。null = 这条消息没什么可再拿到的文件 */
    val path: String?,
    /** 已存到「下载/AnonChat」的位置，例如 下载/AnonChat/a.pdf */
    val saved: String?,
) {
    val file: java.io.File? get() = path?.let { java.io.File(it).takeIf { f -> f.exists() } }
}

/** 点开一张图/一段视频时交给页面层的信息 */
private data class MediaBubble(
    val path: String,
    val video: Boolean,
    val name: String,
)

/** 菜单里的一行操作 */
@Composable
private fun MenuRow(label: String, onClick: () -> Unit) {
    TextButton(onClick = onClick, modifier = Modifier.fillMaxWidth()) {
        Text(label, modifier = Modifier.fillMaxWidth())
    }
}

/**
 * 底部面板里的一格：圆角方块 + 图标 + 文字。
 *
 * 做成方块而不是一排文字按钮，是因为手指在屏幕下方时可用面积大、误触少；
 * 图标区 56dp 也够大，单手能点到。
 */
/** 阅后即焚档位的显示文案。原来住在 Burn（纯逻辑对象）里，i18n 后文案归 UI */
@Composable
private fun burnLabel(opt: Long): String = when (opt) {
    0L -> stringResource(R.string.burn_off)
    30L -> stringResource(R.string.burn_30s)
    else -> stringResource(R.string.burn_5m)
}

@Composable
private fun AttachTile(label: String, icon: String, onClick: () -> Unit) {
    val skin = LocalSkin.current
    Column(
        horizontalAlignment = Alignment.CenterHorizontally,
        modifier = Modifier.width(72.dp),
    ) {
        Box(
            Modifier.size(56.dp).clip(RoundedCornerShape(10.dp)).background(skin.chip)
                .clickable(onClick = onClick),
            contentAlignment = Alignment.Center,
        ) { Text(icon, fontSize = 24.sp) }
        Spacer(Modifier.height(6.dp))
        Text(label, fontSize = 12.sp, color = skin.bubbleText)
    }
}

/** 接收进度条。收齐后这条会消失，换成正常的文件气泡 */
@Composable
private fun RxProgress(f: Repo.FileRx) {
    Column(Modifier.fillMaxWidth()) {
        Text(
            when {
                f.isVoice -> stringResource(R.string.rx_voice, f.percent)
                f.isImage -> stringResource(R.string.rx_image, f.percent)
                f.isVideo -> stringResource(
                    R.string.rx_video, f.percent, formatSize(f.bytes), formatSize(f.size)
                )
                else -> stringResource(
                    R.string.rx_file, f.name, f.percent, formatSize(f.bytes), formatSize(f.size)
                )
            },
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.fillMaxWidth(),
            textAlign = TextAlign.Center
        )
        LinearProgressIndicator(
            progress = { f.percent / 100f },
            modifier = Modifier.fillMaxWidth().padding(top = 4.dp)
        )
    }
}

/** 用系统方式打开本机副本（私有目录必须经 FileProvider 暴露） */
private fun openDownloadedFile(ctx: android.content.Context, path: String) {
    runCatching {
        val f = java.io.File(path)
        val uri = androidx.core.content.FileProvider.getUriForFile(
            ctx, ctx.packageName + ".files", f
        )
        val mime = ctx.contentResolver.getType(uri) ?: "application/octet-stream"
        val i = android.content.Intent(android.content.Intent.ACTION_VIEW)
            .setDataAndType(uri, mime)
            .addFlags(android.content.Intent.FLAG_GRANT_READ_URI_PERMISSION)
        ctx.startActivity(i)
    }.onFailure {
        // 没有能打开这种类型的 App 是常见情况（比如 .bin / .zip），
        // 静默失败会让人以为按钮坏了
        android.widget.Toast.makeText(
            ctx, ctx.getString(R.string.file_no_app), android.widget.Toast.LENGTH_SHORT
        ).show()
    }
}
